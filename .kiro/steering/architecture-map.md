---
inclusion: always
---

# Project Architecture Map

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        CLI Layer                            │
│  cli/mo_agent_api.py (user)  cli/mo_admin_api.py (admin)   │
│  cli/edge_chat_loop.py       cli/tools/  cli/ui/           │
└──────────────────────────┬──────────────────────────────────┘
                           │ HTTP / Direct
┌──────────────────────────▼──────────────────────────────────┐
│                       API Layer                             │
│  api/main.py → api/routers/ → api/services/                │
│                                api/repositories/            │
│  api/models/  api/database.py  api/dependencies.py          │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                      Core Layer                             │
│                                                             │
│  ┌─────────┐  ┌──────────┐  ┌──────────┐  ┌────────────┐  │
│  │  Agent   │  │  Skills  │  │  Events  │  │  Memory    │  │
│  │ Engine   │  │  System  │  │  System  │  │  System    │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └─────┬──────┘  │
│       │             │             │               │          │
│  ┌────▼─────┐  ┌────▼─────┐  ┌────▼─────┐  ┌─────▼──────┐  │
│  │ Context  │  │ Learning │  │ Sandbox  │  │ Evaluation │  │
│  │ Window   │  │ Selector │  │ Branch   │  │ Gate       │  │
│  └──────────┘  └──────────┘  └──────────┘  └────────────┘  │
│                                                             │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────────┐  │
│  │   LLM    │  │ Verify   │  │ Replay   │  │ Streaming  │  │
│  │ Routing  │  │ Firewall │  │ Engine   │  │ Protocol   │  │
│  └──────────┘  └──────────┘  └──────────┘  └────────────┘  │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                    Data Layer                                │
│  MatrixOne (HTAP DB)          Redis (Cache)                 │
│  - 99% MySQL compatible       - Session cache               │
│  - Git for Data (snapshot,    - Rate limiting               │
│    time-travel, branch/merge) - Pub/sub                     │
│  - Vector search (IVF/HNSW)                                 │
│  - Fulltext search                                          │
│  - OLTP + OLAP in one DB                                    │
└─────────────────────────────────────────────────────────────┘
```

---

## Directory Structure

```
mo-dev-agent/
├── api/                    # REST API (FastAPI)
│   ├── main.py             # App entry point, middleware, startup
│   ├── database.py         # SQLAlchemy engine, session factory
│   ├── dependencies.py     # FastAPI dependency injection
│   ├── sse_errors.py       # SSE error handling
│   ├── models/             # SQLAlchemy ORM models (DB tables)
│   │   ├── agent.py        # Agent, Session, Event tables
│   │   ├── auth.py         # User, Role, UserRole tables
│   │   ├── skill.py        # SkillRegistry, SkillInstallation
│   │   ├── context.py      # ContextSnapshot, Decision
│   │   ├── evaluation.py   # EvalGateResult, GoldenSession
│   │   ├── memory.py       # MemoryEntry, MemoryIndex
│   │   ├── workflow.py     # WorkflowDef, WorkflowRun
│   │   ├── verification.py # VerificationResult
│   │   └── infra.py        # InfraConfig
│   ├── routers/            # API endpoint handlers
│   │   ├── chat.py         # POST /chat, /chat/stream
│   │   ├── auth.py         # /auth/register, /auth/login
│   │   ├── agents.py       # CRUD /agents
│   │   ├── sessions.py     # CRUD /sessions
│   │   ├── events.py       # CRUD /events, causal chains
│   │   ├── skills.py       # /skills registry
│   │   ├── sandbox.py      # /sandbox management
│   │   ├── replay.py       # /sessions/{id}/replay
│   │   ├── context.py      # /context snapshots
│   │   ├── decisions.py    # /decisions audit
│   │   ├── jobs.py         # /jobs background tasks
│   │   ├── workflows.py    # /workflows definitions & runs
│   │   ├── triggers.py     # /triggers webhook & cron
│   │   ├── streaming.py    # SSE streaming endpoints
│   │   ├── learning.py     # /learning insights
│   │   ├── evaluation.py   # /evaluation gates
│   │   ├── marketplace.py  # /marketplace skill sharing
│   │   ├── branches.py     # /branches data versioning
│   │   ├── models.py       # /models LLM management
│   │   ├── admin.py        # Admin-only endpoints
│   │   ├── introspection.py # Agent self-inspection
│   │   └── data_versioning.py # Time-travel queries
│   ├── services/           # Business logic layer
│   │   ├── agent_service.py
│   │   ├── session_service.py
│   │   ├── event_service.py
│   │   ├── context_service.py
│   │   ├── decision_service.py
│   │   ├── sandbox_service.py
│   │   ├── replay_service.py
│   │   └── skill_service.py
│   └── repositories/       # Data access layer
│       ├── agent_repository.py
│       ├── session_repository.py
│       ├── event_repository.py
│       ├── user_repository.py
│       └── decision_repository.py
│
├── core/                   # Core business logic
│   ├── agent/              # 🧠 Agent execution engine
│   │   ├── run_engine.py       # Main run orchestrator (run_id lifecycle)
│   │   ├── chat_loop.py        # Plan-Act-Observe-Reflect loop
│   │   ├── execution_backend.py # Tool execution backend
│   │   ├── async_tools.py      # Async tool execution
│   │   ├── agent_registry.py   # Agent definitions
│   │   ├── seed_agents.py      # Default agent configs
│   │   ├── triggers.py         # Webhook/cron triggers
│   │   ├── coordination.py     # Multi-agent coordination
│   │   ├── streaming_output_handler.py # Stream formatting
│   │   ├── stream_processor.py # Stream event processing
│   │   └── stream_validator.py # Stream validation
│   │
│   ├── agents/             # 🤝 Multi-agent collaboration
│   │   ├── scheduler.py       # Agent scheduling
│   │   ├── task_board.py       # Shared task board
│   │   ├── routing.py          # Request routing
│   │   ├── conflict_resolver.py # Conflict resolution
│   │   └── consistency.py      # Cross-agent consistency
│   │
│   ├── skills/             # 🔧 Skill system (LARGEST module)
│   │   ├── registry.py         # Skill version registry
│   │   ├── catalog.py          # Skill catalog & discovery
│   │   ├── loader.py           # Load skills from disk/DB
│   │   ├── skill_md.py         # Markdown skill definitions
│   │   ├── skill_manager.py    # Install/uninstall lifecycle
│   │   ├── skill_index.py      # Skill search index
│   │   ├── selector.py         # Skill selection logic
│   │   ├── modern_selector.py  # Improved selector
│   │   ├── self_improving_selector.py # Learning selector
│   │   ├── learning_signals.py # Learning signal types
│   │   ├── learning_config.py  # Learning configuration
│   │   ├── learning_similarity.py # Similarity matching
│   │   ├── resolver.py         # Dependency resolution
│   │   ├── dependencies.py     # Skill dependencies
│   │   ├── version.py          # Version management
│   │   ├── runner.py           # Skill execution
│   │   ├── pipeline.py         # Skill pipeline
│   │   ├── scaffold.py         # Skill scaffolding
│   │   ├── base.py             # Base skill class
│   │   ├── builtin.py          # Built-in skills
│   │   ├── extended.py         # Extended skills
│   │   ├── delegation.py       # Skill delegation
│   │   ├── mocking.py          # Side-effect isolation
│   │   ├── mcp_bridge.py       # MCP protocol bridge
│   │   ├── data_bridge.py      # Data bridge for skills
│   │   ├── credential_manager.py # Skill credentials
│   │   ├── github_client.py    # GitHub API client
│   │   ├── markdown_skill.py   # Markdown-based skills
│   │   └── procedural_memory.py # Procedural memory
│   │
│   ├── events/             # 📝 Event sourcing system
│   │   ├── event_logger.py     # Create events (write path)
│   │   ├── event_reader.py     # Query events (read path)
│   │   ├── session_manager.py  # Session lifecycle
│   │   ├── causal_chain.py     # Causal chain tracking
│   │   ├── pipeline.py         # Event processing pipeline
│   │   ├── batch_writer.py     # Batch event writing
│   │   ├── embedding_worker.py # Event embedding generation
│   │   ├── models.py           # Event data models
│   │   └── session_models.py   # Session data models
│   │
│   ├── memory/             # 🧠 Memory system
│   │   ├── store.py            # Memory storage
│   │   ├── retriever.py        # Memory retrieval (vector + fulltext)
│   │   ├── tiered_loader.py    # Tiered memory loading
│   │   ├── governance.py       # Lifecycle: decay, quarantine, compress
│   │   ├── provenance.py       # Memory provenance tracking
│   │   ├── typed_pipeline.py   # Typed memory pipeline
│   │   ├── typed_observer.py   # Memory change observer
│   │   ├── session_summary.py  # Session summarization
│   │   ├── profile.py          # User/agent profiles
│   │   ├── sandbox.py          # Memory sandbox
│   │   ├── explain.py          # Memory explainability
│   │   ├── sensitivity.py      # Sensitivity classification
│   │   ├── health.py           # Memory health checks
│   │   ├── prompts.py          # Memory-related prompts
│   │   ├── types.py            # Memory type definitions
│   │   ├── config.py           # Memory configuration
│   │   └── metrics.py          # Memory metrics
│   │
│   ├── context/            # 📋 Context window management
│   │   ├── manager.py          # Context window orchestrator
│   │   ├── prompt_assembler.py # Assemble final prompt
│   │   ├── budget_manager.py   # Token budget allocation
│   │   ├── zone_budgets.py     # Per-zone budget management
│   │   ├── scorer.py           # Relevance scoring
│   │   ├── hybrid_retrieval.py # Vector + keyword search
│   │   ├── embeddings.py       # Embedding operations
│   │   ├── compaction.py       # Context compaction
│   │   ├── history_compression.py # History compression
│   │   ├── pollution.py        # Context pollution detection
│   │   ├── reference_tracking.py # Reference tracking
│   │   ├── prompt_integration.py # Prompt integration
│   │   ├── prompt_optimizer.py # Prompt optimization
│   │   ├── prompts.py          # System prompts
│   │   ├── scratchpad.py       # Working memory
│   │   ├── few_shot.py         # Few-shot examples
│   │   ├── implicit_feedback.py # Implicit feedback
│   │   ├── procedural_hints.py # Procedural hints
│   │   ├── lifecycle.py        # Context lifecycle
│   │   └── scheduler.py        # Context scheduling
│   │
│   ├── llm/                # 🤖 LLM integration
│   │   ├── client.py           # LLM API client
│   │   ├── router.py           # Model routing logic
│   │   ├── model_resolver.py   # Resolve model by scope
│   │   ├── providers.py        # Provider implementations
│   │   ├── rate_limiter.py     # Rate limiting
│   │   ├── models.py           # Model definitions
│   │   ├── seed_models.py      # Default model configs
│   │   └── constants.py        # LLM constants
│   │
│   ├── verification/       # ✅ Trust & verification
│   │   ├── firewall.py         # Hallucination firewall
│   │   ├── claim_extractor.py  # Extract claims from LLM output
│   │   ├── llm_claim_extractor.py # LLM-based extraction
│   │   ├── structured_verifier.py # Structured verification
│   │   ├── streaming_verifier.py # Stream verification
│   │   ├── tool_quality.py     # Tool output quality
│   │   ├── hitl_policy.py      # Human-in-the-loop policy
│   │   └── cot_audit.py        # Chain-of-thought audit
│   │
│   ├── evaluation/         # 📊 Quality evaluation
│   │   ├── regression_gate.py  # Regression gate
│   │   ├── gate_trigger.py     # Gate trigger conditions
│   │   ├── drift_detector.py   # Model drift detection
│   │   ├── drift_pipeline.py   # Drift pipeline
│   │   ├── auto_scorer.py      # Automatic scoring
│   │   ├── quality_scorer.py   # Quality scoring
│   │   ├── multi_level_scorer.py # Multi-level scoring
│   │   ├── confidence_calibrator.py # Confidence calibration
│   │   ├── context_budget_tuner.py # Budget tuning
│   │   ├── training_data.py    # Training data pipeline
│   │   ├── prompt_evolution.py # Prompt evolution
│   │   ├── golden_selector.py  # Golden session selection
│   │   ├── adversarial.py      # Adversarial testing
│   │   ├── slo_monitor.py      # SLO monitoring
│   │   └── gate_cli.py         # Gate CLI commands
│   │
│   ├── sandbox/            # 📦 Sandbox isolation
│   │   ├── sandbox.py          # Sandbox management
│   │   ├── branch.py           # Git-like data branching
│   │   ├── cleanup.py          # Sandbox cleanup
│   │   └── cost_predictor.py   # Cost prediction
│   │
│   ├── replay/             # 🔄 Session replay
│   │   ├── engine.py           # Replay engine
│   │   ├── time_machine.py     # Time-travel queries
│   │   └── semantic_diff.py    # Semantic diff comparison
│   │
│   ├── streaming/          # 📡 Streaming protocol
│   │   ├── agui_protocol.py    # AG-UI protocol events
│   │   ├── multi_agent_aggregator.py # Multi-agent streams
│   │   └── websocket_transport.py # WebSocket transport
│   │
│   ├── scheduling/         # ⏰ Task scheduling
│   │   ├── task_scheduler.py   # Task scheduler
│   │   ├── workflow_engine.py  # Workflow execution
│   │   └── trigger_rules.py    # Trigger rule evaluation
│   │
│   ├── data_versioning/    # 📚 Data versioning
│   │   ├── training_data_pipeline.py # Training data
│   │   ├── knowledge_regression.py   # Knowledge regression
│   │   └── prompt_experiment.py      # Prompt experiments
│   │
│   ├── auth/               # 🔐 Authentication
│   │   ├── user_manager.py     # User CRUD
│   │   ├── jwt_manager.py      # JWT token management
│   │   ├── auth_handlers.py    # Auth middleware
│   │   ├── permission_checker.py # Permission checks
│   │   ├── audit_logger.py     # Audit logging
│   │   ├── password.py         # Password hashing
│   │   ├── encryption.py       # Encryption utilities
│   │   └── seed_roles.py       # Default roles
│   │
│   ├── repos/              # 📁 Multi-repo management
│   │   ├── registry.py         # Repo registry
│   │   ├── token_resolver.py   # Token resolution
│   │   ├── models.py           # Repo models
│   │   └── token_models.py     # Token models
│   │
│   ├── trust_safety/       # 🛡️ Trust & safety
│   │   ├── confidence_scorer.py # Confidence scoring
│   │   └── streaming_verifier.py # Stream verification
│   │
│   ├── embedding/          # 🔢 Embedding system
│   │   ├── client.py           # Embedding client
│   │   └── providers.py        # Embedding providers
│   │
│   ├── runtime/            # 🏃 Code execution runtimes
│   │   ├── docker_runtime.py   # Docker execution
│   │   ├── firecracker_runtime.py # Firecracker VM
│   │   └── subprocess_runtime.py # Local subprocess
│   │
│   ├── code_executor/      # 💻 Code execution
│   │   ├── data_context.py     # Data context for code
│   │   └── security.py         # Execution security
│   │
│   ├── jobs/               # 📋 Background jobs
│   │   ├── runner.py           # Job runner
│   │   ├── router.py           # Job routing
│   │   ├── backend.py          # Job backend
│   │   └── local.py            # Local job execution
│   │
│   ├── workflow/            # 🔀 Workflow engine
│   │   └── engine.py           # Workflow execution
│   │
│   ├── learning/           # 📖 Learning system
│   │   └── input_face_learner.py # Input face learning
│   │
│   ├── scope/              # 🔍 Scope resolution
│   ├── models/             # 📐 Shared models
│   ├── utils/              # 🔧 Utilities
│   │
│   ├── git_for_data.py     # Git-for-data operations
│   ├── exceptions.py       # Core exceptions
│   ├── validation.py       # Input validation
│   ├── cache.py            # Caching layer
│   ├── rate_limit.py       # Rate limiting
│   ├── history_utils.py    # History utilities
│   ├── metrics.py          # Metrics collection
│   └── logging_config.py   # Logging configuration
│
├── cli/                    # Command-line interface
│   ├── mo_agent_api.py     # User CLI (mo-agent)
│   ├── mo_admin_api.py     # Admin CLI (mo-admin)
│   ├── edge_chat_loop.py   # Local chat loop (edge mode)
│   ├── api_client.py       # HTTP client for API
│   ├── permissions.py      # CLI permission management
│   ├── profile_manager.py  # User profile management
│   ├── tools/              # CLI tool implementations
│   │   ├── base.py             # Base tool class
│   │   ├── router.py           # Tool routing
│   │   ├── file_ops.py         # File operations
│   │   ├── search.py           # Code search
│   │   ├── git.py              # Git operations
│   │   ├── shell.py            # Shell execution
│   │   ├── reflect.py          # Self-reflection
│   │   └── introspection.py    # Agent introspection
│   └── ui/                 # CLI UI components
│       ├── renderer.py         # Output rendering
│       ├── markdown.py         # Markdown rendering
│       ├── repl.py             # REPL interface
│       ├── status_bar.py       # Status bar
│       ├── theme.py            # Color theme
│       └── doctor.py           # Health diagnostics
│
├── skills/                 # Skill definitions
│   ├── github/             # GitHub integration skill
│   ├── knowledge/          # Knowledge management skill
│   ├── feedback_classifier/ # Feedback classification
│   ├── feedback_trainer/   # Feedback training
│   ├── evaluate_session/   # Session evaluation
│   └── tune_performance/   # Performance tuning
│
├── schemas/                # Pydantic request/response schemas
├── config/                 # Application configuration
├── tests/                  # Test suite (820+ tests)
├── deployment/             # Docker, K8s, monitoring
├── scripts/                # Dev/ops scripts
├── docs/                   # Documentation
├── plans/                  # Development plans
└── review/                 # Code review records
```

---

## Data Flow: Chat Request

```
User sends message
    │
    ▼
api/routers/chat.py          # Receive HTTP request
    │
    ▼
core/agent/run_engine.py     # Create run, orchestrate
    │
    ├──▶ core/events/event_logger.py    # Log user_query event
    │
    ├──▶ core/context/manager.py        # Build context window
    │       ├── core/memory/retriever.py     # Retrieve memories
    │       ├── core/context/scorer.py       # Score relevance
    │       └── core/context/prompt_assembler.py # Assemble prompt
    │
    ├──▶ core/skills/selector.py        # Select skill
    │       └── core/skills/self_improving_selector.py # Learn
    │
    ├──▶ core/llm/client.py             # Call LLM
    │       └── core/llm/router.py           # Route to provider
    │
    ├──▶ core/verification/firewall.py  # Verify response
    │
    ├──▶ core/events/event_logger.py    # Log llm_response event
    │
    └──▶ core/streaming/agui_protocol.py # Stream to client
```

---

## Data Flow: Event Sourcing

```
Any state change
    │
    ▼
core/events/event_logger.py     # Create event with:
    │                            #   - event_type
    │                            #   - user_id
    │                            #   - session_id
    │                            #   - causal_chain_id
    │                            #   - parent_event_id
    │                            #   - content + metadata
    ▼
core/events/pipeline.py         # Process event:
    │                            #   - Embedding generation
    │                            #   - Memory extraction
    │                            #   - Trigger evaluation
    ▼
MatrixOne DB                     # Persist to conversation_events
    │                            #   - Time-travel queryable
    │                            #   - Zero-copy branchable
    ▼
core/events/event_reader.py     # Query events:
                                 #   - By session
                                 #   - By causal chain
                                 #   - By time range
                                 #   - Cross-session
```

---

## Key Module Relationships

### Agent → Skills → LLM
```
core/agent/chat_loop.py
    → core/skills/selector.py (choose skill)
    → core/skills/runner.py (execute skill)
    → core/llm/client.py (call LLM)
    → core/agent/execution_backend.py (execute tools)
```

### Context → Memory → Events
```
core/context/manager.py
    → core/memory/retriever.py (get memories)
    → core/events/event_reader.py (get events)
    → core/context/scorer.py (rank by relevance)
    → core/context/prompt_assembler.py (build prompt)
```

### Verification → Trust → Audit
```
core/verification/firewall.py
    → core/verification/claim_extractor.py (extract claims)
    → core/trust_safety/confidence_scorer.py (score confidence)
    → core/auth/audit_logger.py (log audit)
```

### Evaluation → Replay → Sandbox
```
core/evaluation/regression_gate.py
    → core/replay/engine.py (replay session)
    → core/sandbox/sandbox.py (isolated environment)
    → core/replay/semantic_diff.py (compare results)
```

---

## Database Tables (MatrixOne)

### Core Tables
- `conversation_events` - All events (event sourcing store)
- `sessions` - Session metadata
- `agents` - Agent definitions
- `users` / `roles` / `user_roles` - Auth

### Skill Tables
- `skill_registry` - Registered skills
- `skill_installations` - Installed skills per user
- `skill_permissions` - Skill permissions
- `skill_selection_events` - Selection audit trail
- `skill_selection_learnings` - Learning data

### Memory Tables
- `memory_entries` - Long-term memory
- `memory_index` - Memory search index

### Evaluation Tables
- `eval_gate_results` - Gate results
- `golden_sessions` - Golden test sessions

### Infrastructure Tables
- `infra_configs` - System configuration
- `audit_logs` - Audit trail

---

## Key Design Decisions

### Why MatrixOne?

MatrixOne ([GitHub](https://github.com/matrixorigin/matrixone)) is a hyper-converged cloud-native HTAP database, 99% MySQL-compatible with unique capabilities:

- **99% MySQL compatible** — use existing MySQL tools, ORMs (SQLAlchemy), drivers. Drop-in replacement
- **Git for Data** — instant snapshots, time-travel queries, branch & merge, instant rollback
- **Vector search** — built-in IVF/HNSW indexing, no external Pinecone/Milvus needed
- **Fulltext search** — built-in boolean/natural language search, no Elasticsearch needed
- **HTAP** — OLTP + OLAP in one database, real-time analytics, no ETL
- **Cloud-native** — storage-compute separation, elastic scaling, Kubernetes-native

**What this means for our project:**
- Time-travel queries → audit trail, context snapshots, "what did the agent see?"
- Zero-copy branching → sandbox isolation, regression testing
- Vector search → memory retrieval, semantic search
- Fulltext search → hybrid retrieval (vector + keyword)
- HTAP → event sourcing (write-heavy) + analytics (read-heavy) in one DB

**⚠️ When encountering unexpected behavior:**
- It's 99% MySQL — most MySQL syntax works as-is
- But it has extra features MySQL doesn't have (snapshots, vectors, etc.)
- If something doesn't work as expected, check [MatrixOne docs](https://docs.matrixorigin.cn/en) first
- Don't assume "it can't be done" — ask the user before giving up

### Why Event Sourcing?
- Every state change is an event → full audit trail
- Causal chains link related events → debuggable
- Replay any conversation → testable
- Time-travel to any point → reconstructable

### Three-Layer Context
```
Memory (infinite) → Selection (scored) → Prompt (finite) → LLM
```
- Memory stores everything
- Selection scores by relevance
- Prompt fits within token budget
- LLM sees only what matters

### Edge vs Cloud
- **Edge (CLI)**: `cli/edge_chat_loop.py` - Local execution with tools
- **Cloud (API)**: `api/routers/chat.py` - Server-side execution
- Both share `core/` logic

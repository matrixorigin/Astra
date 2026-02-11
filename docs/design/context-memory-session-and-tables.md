# Context, Memory, Session Management, Table Design, and Token Management

This document defines the **first-step design** for mo-agent-engine: how context is assembled for the LLM, how memory and sessions work, which tables live in MatrixOne, and how tokens (repo and LLM) are stored and managed. The goal is a clear, implementable design that stays **open to change** as we learn from the MVP.

**Scope**: Design only. Implementation follows in subsequent steps. Tables and abstractions are expected to evolve. This revision incorporates **review feedback** and a **event-centric data-asset** evolution: conversation data is treated as **analyzable, reproducible, and trainable** enterprise assets, with atomic events, versioned configs, evaluation/training pipeline, and MatrixOne as the single persistence layer.

**Design philosophy**: This design treats conversation as **auditable, evolvable, and trainable** enterprise data assets—not as ephemeral context. Every decision is oriented toward “**ten years from now we can still precisely reproduce today’s decision**.” Effect and operational convenience are prioritized; optional compliance features (e.g. 脱敏) can be added later without changing the core model.

---

## 0. Design Principles (Review Alignment)

- **Stable interface, mutable implementation**: Context assembly, memory read/write, and token resolution expose stable APIs; storage and section logic can change behind them.
- **Explicit over implicit**: Token budget allocation, skill selection, and token resolution order are specified in this doc so that implementations and tests align.
- **Fail-safe and observable**: Token overflow, retrieval timeouts, and token failures have defined behavior (truncate, degrade, alert). Key paths are instrumented for metrics.
- **Minimal schema for maximum evolution**: New columns (e.g. `embedding_status`, `summary_status`) support future features (RAG, async summarization) without requiring big migrations later.

### 0.1 Design Evolution: Event-Centric Data Asset

We adopt an **event-centric** model so that:

| Goal | Design choice |
|------|----------------|
| **历史资产化** | Every interaction is an atomic **event** (conversation_events); long-term retention, partitioning, and export pipeline support analysis and training. |
| **跨会话串联** | **user_id** on every event; unified event stream so history can be queried by user across sessions and agents (`SELECT * FROM conversation_events WHERE user_id=? ORDER BY created_at`). |
| **抗变化** | **Versioned configs** (prompt_templates, skills_registry, agent_configs); events reference **prompt_template_id+version** and **skills_snapshot** so historical data is always reproducible with the config that was used. |
| **业务解耦** | **metadata** uses a **namespace convention** (e.g. `dev.code_path`, `chat.topic`); business-specific annotations can live in separate tables or namespaced keys so core schema stays stable. |
| **溯源** | **context_snapshot** (JSON) on each LLM-related event captures template id, skills used, history event ids, retrieved chunks; any event can be fully reproduced. |
| **评分训练** | **event_evaluations** (user/auto scores) and **training_annotations** (labels, dataset_split); **training_eligible** + **quality_score** on events; export pipeline for SFT/RLHF. |
| **MatrixOne 全栈** | All metadata, events, and configs in MatrixOne; vectors only as **embedding_ref** (external store); no dependency on other stores for core analytics. |
| **确定性边界控制** | Agent Decision = LLM(versioned_prompt, versioned_skill, versioned_context, versioned_memory, fixed_params); Git for Data controls 4 of 5 inputs, constraining LLM non-determinism to auditable range. |
| **幻觉防火墙** | hallucination_checks table records every verification; claims verified against same snapshot used for context assembly. |
| **回归门禁** | gate_results table records every quality gate; snapshot-isolated regression testing before any change reaches production. |
| **训练数据版本化** | training_datasets table with snapshot_name as version; full lineage from events to datasets. |

**Core shift**: Conversation is no longer “ephemeral context” but **traceable, analyzable, trainable event data**. Business value (e.g. code-review summary) is produced by upper layers via metadata or views; the core event store stays generic and evolution-resistant.

### 0.2 Industry References and Alignment

The design aligns with or borrows from several established industry approaches, while keeping MatrixOne as the single persistence layer and event-centric causality as the core differentiator:

| Reference | Core idea | Alignment with mo-agent-engine |
|-----------|-----------|-----------------------------|
| **MemGPT** | Memory as OS: core / recall / archival layers; eviction and summarization; "infinite context" via managed flow. | Our **Memory–Prompt–Context** three-layer model and **Token Budget Manager** map to a similar hierarchy; **adaptive compression** (§2.4) and post-conversation **knowledge extraction** mirror eviction/summarization. Long-term = **embedding_ref** + external vector store. |
| **Redis Agent Memory / LangGraph** | Short-term (session context) + long-term (summaries, vectors); TTL, vector search; graph-like state and checkpoints for replay. | **conversation_events** + **sessions** = short-term; **memory_index_queue** + vector store = long-term. **causal_chain_id** and **context_snapshot** provide checkpoint/replay without tying to a specific runtime. **multi_agent_message** supports LangGraph-style multi-agent workflows. |
| **BCG Agent Framework** | Versioned prompts/tools, event listeners, full trajectory logging, eval layers (output/trajectory/step/safety). | **prompt_templates** and **skills_registry** are versioned; events reference **prompt_template_id+version**. **event_evaluations** (source: user/auto/human) and **context_snapshot** (template, skills, history_events, retrieved_chunks) provide trajectory-level traceability. Sandbox + regression gate = controlled release. |
| **Mobisoft Context Engineering** | Context layers: persistent memory, focal selection, compression, isolation; RAG + tool orchestration; anti–context-poisoning. | **build_context** and allocation priority (current task → memory → recent conversation → system/skills) implement a **context pyramid**. Optional **pre-injection validation** (see §1.3) and **dynamic skill filtering** reduce noise and poisoning risk. |
| **AgentOps / SRE** | Versioning, reproducibility, IaC for experiments; Prometheus/metrics. | **Time-point sandbox** with Git for Data (or clone) = reproducible experiments; **SemVer** recommended for template/skill versions; **observability** (§6) and **risk** (§8) tables align with SRE practices. |

These references support **production-grade reliability, auditability, and evolution** without changing the event-centric, MatrixOne-first core.

---

## 1. Context Design

**Context** is the structured input we build for each LLM call. Assembly is **dynamic**: budget-driven truncation, optional skill filtering, and configurable templates.

### 1.1 What Context Contains (Logical Sections)

- **System / identity**: Who the agent is; fixed or configurable per deployment.
- **Available skills**: Subset of skills relevant to the **current task** (see 1.4); name, short description, parameters.
- **Current task**: The user’s current request (highest priority for token budget).
- **Recent conversation**: Last N turns in the current session; **token-count bounded** (see 1.3).
- **Retrieved memory** (optional, later): RAG results; bounded tokens; **timeout with fallback** (e.g. 800ms then omit).
- **User / session summary** (optional, later): Prior-session or user summary.
- **Workspace / repo** (optional): Current repo, paths, normalization hints.

Sections are ordered so that **current task** and **retrieved memory** (when present) are preserved when budget is tight; **recent conversation** is truncated by dropping oldest turns or by token-count truncation.

### 1.2 Token Budget Manager and Context Pyramid

- **Input**: Total token budget (e.g. from config or model limit minus safety margin).
- **Context pyramid** (general → specific): The assembly order forms a pyramid—**base** = system + skills (stable, low variance); **middle** = retrieved memory + current task (dynamic but bounded); **top** = recent conversation (most specific, sliding window). When budget is tight, truncation drops from the top first so that current task and memory stay intact.
- **Allocation priority** (highest to lowest):  
  1. Current task (reserve minimum, then use remainder)  
  2. Retrieved memory (if any; cap per chunk)  
  3. Recent conversation (sliding window; oldest turns dropped first)  
  4. System + skills (fixed or low variance)
- **Mechanics**:  
  - When assembling “recent conversation”, use **conversation_events.token_usage** or **token_count** (stored at write time) to avoid real-time tokenizer calls.  
  - Sum token counts until budget is exhausted; truncate or drop oldest events as needed.  
  - If total still over budget after allocation, **truncate “recent conversation”** and log a warning; never send over-budget context to the LLM. Return a user-facing message if context had to be heavily truncated (e.g. “Long conversation was shortened; consider starting a new session.”).
- **Config**: e.g. `context_max_tokens`, `context_current_task_min_tokens`, `context_history_max_tokens` in `configs`.
- **Cost and stability**: Monitor **context token variance** (e.g. target &lt;10% fluctuation for similar sessions); **compression** (truncation + optional summarization) keeps usage within 20–30% of unconstrained growth. Model-agnostic abstraction (e.g. via configurable model id) avoids vendor lock-in.

**Token Budget Allocation Algorithm**:

Given `total_budget` (from `configs.context_max_tokens`, e.g. 8000 tokens):

```
allocate(total_budget, has_rag):
  # Step 1: Reserve fixed sections
  system_skills_cap  = min(configs.context_system_skills_max_tokens, total_budget * 0.15)  # e.g. 1200
  current_task_min   = configs.context_current_task_min_tokens  # e.g. 800, hard floor

  # Step 2: Reserve current task minimum
  remaining = total_budget - system_skills_cap - current_task_min

  # Step 3: Allocate RAG (if enabled)
  if has_rag:
    rag_cap = min(configs.context_rag_max_tokens, remaining * 0.30)  # e.g. max 1500
    remaining -= rag_cap
  else:
    rag_cap = 0

  # Step 4: Remaining goes to recent conversation
  history_cap = remaining  # whatever is left

  # Step 5: Current task gets its minimum + any unused from other sections
  current_task_cap = current_task_min  # may grow if other sections underuse

  return { system_skills_cap, current_task_cap, rag_cap, history_cap }
```

**Truncation rules** (applied during build_sections):
- **System + skills**: If rendered text exceeds `system_skills_cap`, drop skills by lowest relevance score until within cap. Never truncate system identity.
- **Current task**: If exceeds `current_task_cap`, truncate from the end with "... [truncated]" marker. Log warning.
- **RAG**: If total retrieved chunks exceed `rag_cap`, drop lowest-similarity chunks first. Each chunk capped at `configs.context_rag_per_chunk_max_tokens` (e.g. 300).
- **Recent conversation**: Load events newest-first; sum `token_usage.total` per event; stop when sum exceeds `history_cap`; drop oldest events that don't fit. If zero events fit, log warning and return "context too constrained" in metadata.
- **Redistribution**: After all sections are built, if any section used less than its cap, the surplus is offered to `history_cap` (most likely to benefit from extra space).

**Config defaults** (in `configs` table, scope_type=global):

| Key | Default | Description |
|-----|---------|-------------|
| `context_max_tokens` | 8000 | Total budget per LLM call |
| `context_system_skills_max_tokens` | 1200 | Cap for system identity + skills list |
| `context_current_task_min_tokens` | 800 | Hard floor for current user request |
| `context_rag_max_tokens` | 1500 | Cap for retrieved memory chunks |
| `context_rag_per_chunk_max_tokens` | 300 | Per-chunk cap |
| `context_history_max_tokens` | (computed) | Remainder after other allocations |

### 1.3 Context Assembly Flow (Stable Interface)

```
build_context(session_id, current_request, options?) -> (context_string, metadata)
  metadata: { total_tokens, section_tokens, truncated, retrieval_ms?, retrieval_hit }
```

1. **Resolve session**: Load or create session; load recent **events** (ordered by created_at DESC) for this session — or for **cross-session** history, by `user_id` with `event_type IN ('user_query','llm_response')` and limit.
2. **Allocate budget**: Token Budget Manager computes per-section caps from total budget.
3. **Load config versions**: Load **prompt_templates** (active version for this agent) and **skills_registry** (versions for this agent); optionally **filter skills by current task** (see 1.4); build “Available skills” within skill-section budget.
   - **Prompt version routing**: Select which template version via routing function:
     - If `options.template_version` is set (explicit pin): use it. Reason: `"explicit_pin"`.
     - If A/B experiment is active for this agent_id (from `configs`, key=`ab_test_{agent_id}`, value=`{versions:["v3","v4"], weights:[0.5,0.5], salt}`): `hash(session_id + salt)` → deterministic bucket → select version. Reason: `"ab_test:exp_id:bucket"`.
     - Else: select row where `is_active=true AND effective_at <= now()`, latest `effective_at` wins. Reason: `"active_latest"`.
   - **Recording**: Resolved `template_id@version` → `conversation_events.prompt_template_id`. Routing reason → `context_snapshot.routing_reason` (e.g. `"ab_test:exp_123:bucket_1"`). This ensures replay can determine "why v2 was used instead of v1".4. **Load memory** (when implemented): If RAG enabled, run retrieval with **timeout (e.g. 800ms)**; on timeout, proceed with empty “Retrieved memory” and record in metadata for monitoring.
5. **Build sections**: For each section, pull from source (events, memory, config), apply truncation using token counts and caps.
6. **Render prompt**: Use **prompt_templates** content (versioned); concatenate sections; support **hot-update and A/B** by template version.
7. **Record snapshot**: Before/after LLM call, persist a **conversation_event** with **context_snapshot** = `{ prompt_template_id, skills_used: [id+version], history_events: [event_id], retrieved_chunks: [chunk_id] }` so the request is **100% reproducible**. Optionally include **injection_meta** (e.g. section order) in context_snapshot for audit.
8. **Optional pre-injection validation**: To reduce context poisoning risk, a hook can validate assembled context before sending to the LLM (e.g. heuristic or LLM-as-judge). On failure, truncate or drop the offending section and log; do not block the request. Implement when security posture requires it.
9. **Log**: Persist assembly metadata in the event’s metadata and context_snapshot for analysis and tuning.

**Prompt template**: Stored in **prompt_templates** (template_id, version, content, effective_at, is_active). Events reference prompt_template_id (e.g. template_id+version) so historical data always binds to the config that was used. Enables hot-update, A/B, and full traceability.

### 1.4 Dynamic Skill Filtering (Optional, Reduce Token Use)

- **Problem**: Full skill list can exceed context budget or distract the model.
- **Approach**: Before building the “Available skills” section, **filter skills** by relevance to the current task:
  - **Lightweight**: Keyword or tag match (e.g. task contains “PR” → include `summarize_pr`, `create_issue`).
  - **Later**: Small classifier or LLM call to classify task and return allowed skill IDs.
- **Fallback**: If no skills match or filtering is disabled, include all available skills (or a configurable default subset).
- **Recording for reproducibility**: The filtering result is recorded in `context_snapshot` to ensure replay fidelity:
  - `context_snapshot.skills_used`: Skills that passed the filter and were included in context (already defined).
  - `context_snapshot.skills_filtered_out`: Skills that were available but excluded by the filter, with reason (e.g. `[{"id":"deploy_k8s","version":"v1","reason":"no_keyword_match"}]`). This enables debugging "why didn't the agent use skill X?" without guessing.
  - `context_snapshot.skill_filter_method`: Which filter was applied (e.g. `"keyword"`, `"classifier_v2"`, `"none"`). If the filter method changes, historical snapshots still record what was used.

### 1.6 Memory, Prompt, and Context: Three-Layer Model

Three concepts are kept **explicitly distinct** so that storage, versioning, and replay stay clear:

| Dimension | Memory (记忆) | Prompt (指令) | Context (上下文) |
|-----------|----------------|----------------|-------------------|
| **Role** | Agent’s **persistent knowledge** (short/medium/long-term) | Agent’s **behavior and role** (rules, identity, style) | **Single inference input**: Memory + Prompt + current task |
| **Source** | conversation_events (short); user/session summaries (medium); memory_chunks + vector store (long) | Human design + A/B → **prompt_templates** (versioned) | **Context builder** assembles: current query, selected history (by user_id/session), RAG results, skills list, system instruction from Prompt |
| **Persistence** | All in MatrixOne (events, summaries, chunk metadata); vectors by ref | All versioned in MatrixOne (**prompt_templates**) | **Snapshot per call** in conversation_events.**context_snapshot** (never mutated) |
| **Evolution** | Short: sliding window/summary; medium: periodic summaries; long: knowledge ingest + vector model updates | New version publish; **historical events stay bound to old version** (reproducibility) | Assembly strategy improves (e.g. retrieval); **historical snapshots never change** (replay consistency) |
| **Replay key** | **vector_db_snapshot_id** (session) ties to vector state at that time | **prompt_template_id+version** on event | **context_snapshot** JSON (all input pieces + metadata) |
| **Sandbox role** | In sandbox: fix wrong memory, replay to verify impact | In sandbox: test new prompt, compare outcomes | In sandbox: observe how new Prompt + new Memory change assembled Context |

**Flow**: Memory + Prompt → **Context** (assembled) → LLM → **New events** → (optionally) **New Memory** (e.g. extracted chunks, summaries). So each conversation both **consumes** Memory and Prompt and **produces** new Memory; Context is the ephemeral input, but its **snapshot** is persisted for replay.

**Post-conversation evolution hook**: After each **causal chain** (one user query and its full LLM/tool chain) completes, the system can trigger:
1. **Scoring**: If quality_score meets threshold (e.g. &gt; 4.5) or user feedback is positive → set **training_eligible** on the chain’s events.
2. **Memory update**: If the chain is “knowledge-heavy” → enqueue **knowledge extraction** (e.g. new memory_chunk, vector index).
3. **Prompt signal**: If user edited the response or gave negative feedback → log **prompt_improvement_signal** (e.g. in a dedicated table or event_evaluations.feedback with source=user_feedback). **Flow**: user negative feedback → prompt_improvement_signal recorded → **human review** → A/B test proposal or template change → **new prompt_templates version** published. This closes the loop from “user didn’t like the answer” to “we tried a new prompt and measured.”

**Innovation (self-healing / semi-automated prompt evolution)**:
- **Prompt evolution in sandbox**: Beyond manual review, the system can run **“prompt evolution experiments”** in the sandbox: use **genetic algorithms** or **LLM-based search** (e.g. DSPy, TextGrad) on historical high-score and low-score causal chains to optimize the prompt, produce **candidate versions**, auto-evaluate them in sandbox, and **recommend merge** to human. This turns “human reviews prompt” into a **semi-automated** flow: human approves or rejects recommended candidates.
- **Causal-chain regression gate**: Before any new prompt/skills version is merged to production, **automatically** replay in sandbox the **last N low-score chains** and **last N high-score chains** with the candidate version; compute **quality delta** (e.g. score change, regression rate). If regression exceeds a configured threshold, **reject merge**. This is stricter than manual spot-checks and makes releases safer.

These hooks are **design points**; implementation can be async (queues, batch jobs) so that the main request path stays fast.

### 1.7 Open Points (Refine Later)

> **Note**: "Exact default token budgets" and "RAG chunks cap" are now defined in §1.2 Token Budget Allocation Algorithm. Remaining open point:
- Exact default token budgets per section.
- Multiple “context profiles” (minimal vs full) per call type.
- Number of RAG chunks and per-chunk token cap.


### 1.8 End-to-End Data Flow

One user request, from ingress to LLM response, touching every component and table:

```
User Request (HTTP/CLI)
  │
  ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 1. Session Resolution                                               │
│    READ  sessions WHERE user_id=? AND status='active'               │
│          ORDER BY last_active_at DESC LIMIT 1                       │
│    If none → CREATE session (session_id, user_id, status='active')  │
│    Output: session_id                                               │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 2. Event Persistence (user_query)                                   │
│    WRITE  conversation_events                                       │
│           event_type='user_query', content=request_text,            │
│           causal_chain_id=event_id (chain starts here),             │
│           parent_event_id=NULL                                      │
│    UPDATE sessions SET last_event_id=?, event_count+=1,             │
│           last_active_at=now()                                      │
│    Output: user_event_id, causal_chain_id                           │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 3. Token Budget Allocation                                          │
│    READ  configs WHERE key='context_max_tokens' (+ other caps)      │
│    Run allocate(total_budget, has_rag) → per-section caps           │
│    Output: { system_skills_cap, current_task_cap, rag_cap,          │
│              history_cap }                                          │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 4. Prompt Version Routing                                           │
│    READ  configs WHERE key='ab_test_{agent_id}' (if A/B active)     │
│    READ  prompt_templates WHERE agent matches, resolve version      │
│    Routing reason recorded: "active_latest"|"ab_test:..."|"pin"     │
│    Output: template_id@version, routing_reason                      │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 5. Skill Filtering                                                  │
│    READ  skills_registry WHERE agent matches                        │
│    Filter by current_task (keyword/classifier)                      │
│    Record: skills_used[], skills_filtered_out[], filter_method       │
│    Truncate to system_skills_cap                                    │
│    Output: filtered skill list                                      │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 6. Memory Retrieval (parallel where possible)                       │
│                                                                     │
│  6a. Short-term:                                                    │
│      READ conversation_events WHERE session_id=?                    │
│           ORDER BY created_at DESC                                  │
│      Sum token_usage.total per event until history_cap exhausted    │
│                                                                     │
│  6b. Long-term (RAG, if enabled):                                   │
│      CALL vector_store.search(query=current_task, user_id,          │
│           top_k, timeout=800ms)                                     │
│      On timeout → skip, record retrieval_hit=false                  │
│      On success → READ conversation_events/memory_chunks            │
│           by embedding_ref for text; truncate to rag_cap            │
│                                                                     │
│  6c. Medium-term (if available):                                    │
│      READ sessions.summary or session_summaries WHERE user_id=?     │
│                                                                     │
│  Output: history_events[], retrieved_chunks[], summary_text?        │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 7. Context Assembly                                                 │
│    Render prompt_template with sections:                            │
│      [system_identity] + [skills_list] + [retrieved_memory]         │
│      + [session_summary] + [recent_conversation] + [current_task]   │
│    Apply per-section truncation from step 3 caps                    │
│    Redistribute surplus tokens to history                           │
│    Total tokens must <= context_max_tokens                          │
│    Output: context_string, section_tokens{}                         │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 8. Event Persistence (llm_request) + Snapshot                       │
│    WRITE  conversation_events                                       │
│           event_type='llm_request',                                 │
│           causal_chain_id (from step 2),                            │
│           parent_event_id=user_event_id,                            │
│           prompt_template_id=template_id@version,                   │
│           llm_model_used, llm_params,                               │
│           context_snapshot={                                        │
│             prompt_template_id, routing_reason,                     │
│             skills_used, skills_filtered_out, skill_filter_method,  │
│             history_events: [event_ids],                            │
│             retrieved_chunks: [chunk_ids],                          │
│             section_tokens, total_tokens, truncated                 │
│           }                                                         │
│    Output: llm_request_event_id                                     │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 9. LLM Call                                                         │
│    READ  tokens WHERE type='llm', resolve by priority               │
│    CALL  LLM API (context_string, llm_params)                       │
│    WRITE token_usage_log (token_id, success, error_code)            │
│    On 401 → UPDATE tokens SET is_active=false; alert                │
│    Output: llm_response_text, usage{prompt,completion,total}        │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 10. Event Persistence (llm_response)                                │
│     WRITE conversation_events                                       │
│           event_type='llm_response', content=response_text,         │
│           token_usage=usage, causal_chain_id, llm_model_used,       │
│           parent_event_id=llm_request_event_id                      │
│     If response contains tool_calls → loop:                         │
│       WRITE event_type='tool_call' (parent=llm_response)            │
│       Execute tool                                                  │
│       WRITE event_type='tool_result' (parent=tool_call,             │
│             content={input,output,error_code})                      │
│       → back to step 7 (re-assemble context with tool results)      │
│     Output: final_response_event_id                                 │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 11. Post-Chain Hooks (async, non-blocking)                          │
│     a. Quality scoring → set quality_score, training_eligible       │
│     b. Memory extraction → enqueue to memory_index_queue            │
│     c. Prompt signal → if negative feedback, log to                 │
│        event_evaluations                                            │
│     d. Session update → last_event_id, event_count                  │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
              Return response to user
```

**Tables touched per request** (typical, no tool calls):

| Step | Read | Write |
|------|------|-------|
| 1 | sessions | sessions (create if new) |
| 2 | — | conversation_events, sessions |
| 3 | configs | — |
| 4 | configs, prompt_templates | — |
| 5 | skills_registry | — |
| 6 | conversation_events, vector_store (external) | — |
| 7 | (in-memory assembly) | — |
| 8 | — | conversation_events |
| 9 | tokens | token_usage_log |
| 10 | — | conversation_events (1-3 events) |
| 11 | — | conversation_events (update), memory_index_queue |

**Critical path latency** (steps 1-10 are synchronous): Session resolve + event writes + context assembly + LLM call. LLM call dominates; context assembly target < 50ms; event writes can be async-acked if needed.
---

## 2. Memory Design

**Memory** supports context: short-term (session turns), medium-term (session summaries), long-term (RAG). MatrixOne stores **metadata and references**; **embeddings and vector search** live in a separate vector store to avoid schema/operational coupling.

### 2.1 Layers (Target State)

| Layer       | Scope           | Storage (MatrixOne + external)        | Usage in context                |
|------------|-----------------|----------------------------------------|---------------------------------|
| Short-term | Current session | MatrixOne `conversation_events`       | “Recent conversation” (last N events) |
| Medium-term| Recent sessions | MatrixOne `sessions.summary_*`, optional summary table | “User summary” (async job)      |
| Long-term  | All history     | MatrixOne metadata + **external vector store** (Chroma/Pinecone etc.) | “Retrieved memory” (RAG)        |

**RAG path (explicit)**:
- **MatrixOne does not store embedding vectors**. It stores: event/chunk identity, `embedding_ref` (external vector store id), and status; vectors live in external store.
- **Vector store**: Deployed separately (e.g. Chroma, Pinecone); stores embeddings and supports similarity search.
- **Pipeline**: Event completed → enqueue for indexing (`memory_index_queue` with event_id) → async worker embeds and writes to vector store → MatrixOne event updated (`embedding_ref` set). Retrieval: query vector store by user/session scope; return chunk_ids/embedding_refs; load text from MatrixOne or cache.

**MemGPT-style alignment**: Short-term = message buffer (conversation_events, sliding window); medium-term = summaries and editable "core" blocks (e.g. user/session summary); long-term = archival (vector store + embedding_ref). **Promotion** from short to long can use **quality_score** and **training_eligible** so high-value events are prioritized for indexing. A dedicated **memory agent** (e.g. a sub-process or async job that runs knowledge extraction and summarization in the post-conversation hook) can be added later to refine and promote memory without blocking the main request path.

### 2.2 What We Store (Now vs Later)

- **Now**: **conversation_events** in MatrixOne with `event_id`, `user_id`, `session_id`, `agent_id`, `agent_version`, `event_type`, `content`, **context_snapshot**, **token_usage**, **prompt_template_id**, **skills_snapshot**, **quality_score**, **is_flagged**, **training_eligible**, `embedding_ref`, `created_at`, `metadata`. **Content** stores the **original** text so that replay and training are faithful; **脱敏 (desensitization) is optional** and can be introduced later for compliance if needed—we prioritize reproducibility and effect first.
- **Medium-term**: **sessions.summary_status** (pending/completed/failed), **summary_job_id**; optional `session_summaries` table. Summary job consumes events and writes summary text.
- **Long-term**: **conversation_events.embedding_ref**; **memory_index_queue** (event_id, status, retry_count) for async indexing; vector store holds actual embeddings. MatrixOne holds only refs and metadata.

### 2.3 Memory Interface (Stable Abstractions)

- **Write**: On each **event** (user_query, llm_request, llm_response, tool_call, tool_result, system_message, **multi_agent_message**), persist to conversation_events with context_snapshot when applicable. Optionally enqueue to `memory_index_queue` for RAG indexing.
- **Read (short-term)**: “Get last N events for session_id” or “for user_id (cross-session)” (index `conversation_events(session_id, created_at DESC)` or `(user_id, created_at DESC)`), with optional filter by event_type.
- **Read (medium-term)**: “Get summary for user_id or session_id” (from sessions or session_summaries); trigger or wait for summary job by summary_status.
- **Read (long-term)**: “Retrieve top-k chunks for query” → call vector store with timeout (e.g. 800ms); on timeout, return empty and record for metrics; load text by embedding_ref from MatrixOne/cache.

### 2.4 Hierarchical Retrieval and Adaptive Compression (Evolution)

To go beyond basic RAG and bounded short-term memory:

- **Hierarchical / advanced retrieval**: Support **HyDE** (hypothetical document embeddings), **multi-vector** (e.g. chunk + summary embedding), or **parent-child chunking** so that retrieval can return the right granularity (e.g. section vs paragraph). MatrixOne still stores only **embedding_ref** and metadata; the vector store and retrieval layer implement the strategy.
- **Adaptive compression**: When **short-term memory** (recent conversation) exceeds the token budget, trigger **automatic summarization** (e.g. compress oldest N events into a summary). The **summarization itself is recorded as an event** (e.g. a dedicated event_type or a system_message with a well-known metadata flag) so that “why this history was compressed” is **auditable** and reproducible. This keeps context usable while staying within budget.

### 2.5 Open Points (Refine Later)

- Embedding model and chunking strategy.
- Retention and archival (when to summarize or drop old turns).
- Per-user vs per-tenant isolation (align with RBAC).

### 2.5.1 RAG Reproducibility Design

The design goal ("ten years from now we can still precisely reproduce today's decision") requires that **RAG retrieval results are reproducible**. Since the vector store is external and mutable (models change, indexes rebuild), we need explicit mechanisms:

**Problem**: `context_snapshot.retrieved_chunks` records **which** chunks were retrieved, but if the embedding model changes or the vector store is rebuilt, the same query may return different chunks. This breaks reproducibility.

**Solution — three layers of defense**:

1. **Snapshot the retrieval result, not just the ref** (Phase 1, mandatory):
   - `context_snapshot.retrieved_chunks` stores not just `[chunk_id]` but `[{chunk_id, embedding_ref, text_hash, similarity_score}]`.
   - `text_hash` (e.g. SHA-256 of the chunk text) allows verification: "is the chunk text still the same as when it was retrieved?"
   - On replay, if the chunk text matches `text_hash`, the retrieval is reproducible. If not, log a **reproducibility_warning** in the replay result.

2. **Version the embedding model** (Phase 3, when RAG is implemented):
   - Add `embedding_model_id` (e.g. `"text-embedding-3-small-20240101"`) to:
     - `memory_index_queue` (which model was used to embed this event)
     - `conversation_events.metadata.rag.embedding_model_id` (which model was used for the query embedding at retrieval time)
   - When the embedding model changes, **do not delete old vectors**. Instead:
     - New events are embedded with the new model and stored with a new `embedding_model_id`.
     - Old vectors remain queryable (if the vector store supports multi-model indexes) or are re-embedded in a background job.
     - `context_snapshot` records the model used, so replay knows "this retrieval used model X".

3. **Vector store snapshot strategy** (Phase 5, for sandbox):
   - **Option A (preferred if vector store supports it)**: Use vector store's native snapshot/backup (e.g. Pinecone collections, Chroma persistence). Store snapshot ref in `sessions.vector_db_snapshot_id`.
   - **Option B (fallback)**: Do not snapshot the vector store. Instead, on sandbox replay:
     - Use `context_snapshot.retrieved_chunks` directly (the text and scores are already recorded).
     - Skip the vector search step; inject the historical chunks into the context as if they were retrieved.
     - Label the replay as `"vector_state=snapshot_from_context"` (not a live re-retrieval).
   - **Option C (full rebuild)**: Replay `memory_index_queue` events up to timestamp T into a fresh vector store instance. Expensive but precise. Use only when Option A is unavailable and Option B is insufficient.

**Decision matrix**:

| Scenario | Strategy | Reproducibility | Cost |
|----------|----------|----------------|------|
| Normal replay (same model, same vectors) | Re-query vector store; verify via text_hash | Exact | Low |
| Model changed since original | Use Option B (inject from snapshot) | Exact (context level) | Low |
| Sandbox at T1 | Option A if available; else Option B | Exact or near-exact | Low-Medium |
| Full audit / compliance | Option C (rebuild vector store at T1) | Exact (vector level) | High |

**Summary table for session_summaries** (medium-term memory reproducibility):

To ensure medium-term memory is also reproducible, session summaries must record their provenance:

```sql
CREATE TABLE session_summaries (
  summary_id     VARCHAR(64) PRIMARY KEY,
  session_id     VARCHAR(64) NOT NULL,
  user_id        VARCHAR(64) NOT NULL,
  summary_text   TEXT NOT NULL,
  source_event_ids JSON NOT NULL,       -- which events were summarized
  summarizer_model VARCHAR(64),         -- which model generated the summary
  summarizer_params JSON,               -- e.g. {"temperature":0, "max_tokens":500}
  token_count    INT,                   -- for budget allocation
  created_at     TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_ss_session ON session_summaries(session_id, created_at DESC);
CREATE INDEX idx_ss_user ON session_summaries(user_id, created_at DESC);
```

This ensures: (a) we know which events went into the summary, (b) we can regenerate it with the same model/params, (c) token_count enables budget allocation without re-tokenizing.

---

### 2.6 Conversation Replay and Causal Chain (“对话时光机”)

To support **precise replay** and **fault attribution**, events form a **causal chain**: each event can point to its parent and to a **causal_chain_id** that groups the full request chain (user_query → llm_request → llm_response → tool_call → tool_result → …).

**Schema addition (conversation_events)**:

- **parent_event_id** (nullable): The immediate prior event in the chain (e.g. llm_response’s parent is llm_request).
- **causal_chain_id** (e.g. ULID): Shared by all events that belong to **one logical request** (one user question and its full LLM/tool response chain). Enables “load all events for this chain” in one query.
- **llm_model_used** (nullable): Model identifier at inference time (e.g. `gpt-4o-2024-05`, `qwen-max`). Needed for reproducibility and for replay-with-different-model.
- **llm_params** (nullable, JSON): e.g. `{"temperature":0.7, "max_tokens":1024}`. Stored when event_type is llm_request/llm_response so replay can use the same or overridden params.

**Index**: `(causal_chain_id, created_at)` for efficient “get full chain in order”.

**Causal chain building rules** (implement at write time to avoid ambiguity):

1. **user_query** event: set `causal_chain_id = event_id` (this event starts the chain); `parent_event_id` = null (or previous chain’s last event if you link chains).
2. **llm_request** / **llm_response**: inherit `causal_chain_id` from the user_query that triggered them; `parent_event_id` = direct predecessor (e.g. llm_response.parent = llm_request).
3. **tool_call**: `parent_event_id` = llm_response that emitted the tool call; same `causal_chain_id`.
4. **tool_result**: `parent_event_id` = corresponding tool_call; same `causal_chain_id`.
5. **system_message**: same `causal_chain_id` as the chain it belongs to; parent as appropriate.
6. **multi_agent_message**: For multi-agent workflows (e.g. AutoGen group chat), events between agent instances use this type; `causal_chain_id` and `parent_event_id` extend naturally across agents so the full collaboration is one traceable chain. Enables future complex workflows and cross-agent replay.

**Integrity**: Events with the same `causal_chain_id` should be written in order (transaction or strict ordering); monitor for **orphan events** (no parent and not user_query) and alert. Same-chain events are written consecutively so no “broken” chain.

**Replay workflow (design)**:

1. **Load chain**: Query events by `session_id` (or by `causal_chain_id`) ordered by `causal_chain_id, created_at`.
2. **Reconstruct context**: For each chain, take the first event’s **context_snapshot**; resolve **prompt_template_id** to the historical template version; optionally resolve **vector_db_snapshot_id** (from session) to load the same vector state.
3. **Re-run**: Render prompt from snapshot + historical template; call LLM with **same** llm_params (pure replay) or with **replacement** model/params (e.g. `target_llm="new-model"`).
4. **Compare**: Return or store original_response vs new_response; optional diff for regression/improvement analysis.

**Use cases**:

| Scenario | Value |
|----------|--------|
| **Fault attribution** | User reports “yesterday’s answer was wrong” → replay that chain with exact context/model/params → determine whether the cause was prompt, memory, or model. |
| **Model iteration** | Replay a large set of historical chains with `target_llm="new-model"` → measure quality/regression before production rollout. |
| **Compliance / audit** | Export full causal chain + context_snapshot + model/params for a given decision (e.g. financial/medical) to prove “what was the input and config at that time”. |
| **Training data** | Replay with multiple models → collect alternative responses → build preference (RLHF) or SFT datasets. |

**Tool-call replay (determinism)**: When replaying a chain, **tool_call** events must **not** re-invoke external APIs. Use the **historical tool_result.content** for that tool_call so the replay is deterministic and repeatable. Therefore **tool_result** events must **fully record** input, output, and error_code (or equivalent) so that downstream steps (e.g. next LLM turn) see the same state as in the original run.

Replay API and tooling (e.g. “replay_session”, “replay_chain”) can be implemented on top of these fields without changing the event schema again.

---

## 3. Session Management

**Session** is one continuous conversation: one user (and optionally tenant), one thread, with lifecycle and **recovery** support.

### 3.1 Session Identity and Scope

- **session_id**, **user_id**, **tenant_id** (optional), **created_at**, **updated_at**, **status**, **metadata** (as before).
- **last_active_at**: Updated on each event; independent of updated_at. Used for “latest active session” and idle timeout.
- **last_event_id** (ref to conversation_events; no FK to avoid circular dependency): Quick reference to the latest event; application-maintained (e.g. async update by event write service).
- **event_count** (or turn_count): Incremented on each append; used for **max_turns_per_session** (or max_events_per_session) enforcement (config) and analytics.
- **summary_status** (optional): pending | completed | failed. **summary_job_id** (optional): Links to async summary job for medium-term memory.

### 3.2 Session Lifecycle and Limits

1. **Create**: First message → create session, set last_active_at, event_count=0.
2. **Append**: Each event → update last_active_at, last_event_id, event_count. If **event_count >= max_events_per_session** (from configs): either **archive** session (status=closed, optionally trigger summarization) or **refuse** new events and return “Session limit reached; start a new session.” (Policy configurable.)
3. **Idle**: Background job or on-next-request: if `now - last_active_at > session_idle_timeout_hours` (from configs, e.g. 24), set status=idle (or closed).
4. **Recovery**: **GET /sessions/latest?user_id=...** returns the most recent session by last_active_at (or updated_at) for that user (and tenant). Client can use this when it has no session_id (e.g. after refresh) to “resume” the same thread.


### 3.2.1 Session State Machine

```
                    first message
         ┌──────────────────────────────┐
         │                              ▼
     (no session)                   [active]
                                   /    |    \
                     event_count  /     |     \  idle timeout
                     >= max      /      |      \  (background job)
                                ▼       |       ▼
                           [closed]     |    [idle]
                              ▲         |       │
                              │         │       │  new message
                              │         │       │  from same user
                              │         │       ▼
                              │         │   [active]  (reactivate)
                              │         │
                              │         └── manual close (API)
                              │                │
                              └────────────────┘
```

**Transition rules**:

| From | To | Trigger | Action |
|------|----|---------|--------|
| (none) | active | First message from user | Create session; set status='active', event_count=0 |
| active | active | New event | Update last_active_at, event_count, last_event_id |
| active | closed | event_count >= max_events_per_session | Set status='closed'; optionally trigger summarization; return "Session limit reached" |
| active | idle | now - last_active_at > session_idle_timeout_hours | Background job sets status='idle' |
| active | closed | Manual close (API call) | Set status='closed' |
| idle | active | New message from same user | Set status='active'; resume appending events |
| idle | closed | Retention policy (e.g. idle > 7 days) | Background job sets status='closed'; trigger summarization |
| closed | (terminal) | — | No new events accepted; read-only; eligible for archival |

### 3.2.2 Concurrency and Multi-Device

**Problem**: Same user may send messages from multiple devices or tabs simultaneously, or rapid-fire messages before the previous response completes.

**Design**:

- **One active session per user** (default policy): `GET /sessions/latest` returns the single active session. If a second device connects, it joins the same session. This is the simplest model and matches most CLI/chat use cases.
- **Concurrent writes to same session**: Events are appended with `causal_chain_id` set at the user_query level. If two user_queries arrive concurrently for the same session:
  - Each gets its own `causal_chain_id` (two independent chains).
  - Both are valid events in the session; `created_at` ordering determines display order.
  - The context assembly for each chain uses the session's events up to that point (read-your-own-writes consistency required).
  - **Risk**: Two concurrent chains may produce conflicting tool actions. **Mitigation**: Application-level lock per session during chain execution (e.g. optimistic lock via `last_event_id` check-and-set). If lock fails, return "Another request is in progress" to the second caller.
- **Multi-session per user** (optional, future): If needed (e.g. user wants parallel conversations), allow multiple active sessions per user. `GET /sessions/latest` returns the most recent; client can also specify `session_id` explicitly. Requires UI support.

**Session recovery across devices**:

```
Client connects (no session_id in local state):
  1. GET /sessions/latest?user_id=U123
  2. If result.status == 'active' AND now - result.last_active_at < session_idle_timeout:
       resume this session (use result.session_id)
  3. Else:
       create new session
```
### 3.3 Config Knobs (in configs)

- **session_idle_timeout_hours** (e.g. 24): For idle/close policy.
- **max_turns_per_session** (e.g. 100): Cap turns; beyond that, archive or reject.

### 3.4 Open Points (Refine Later)

- “Fork session” or “branch conversation”.
- Exact archival and summarization trigger.
- **Fork session**: API to create a new session that branches from a given session at a given event (copy events up to that point). Enables branch conversation for sandbox or user-facing try-another-path. Design: new session_id, shared or copied events up to fork point; then independent append.
- **Event-driven / async**: causal_chain_id can be produced or consumed via an async queue (e.g. Kafka, Redis Streams); ordering and exactly-once are application responsibilities. Async is an optional scaling pattern.

### 3.5 Time-Point Sandbox (“平行宇宙实验台”)

To run **offline experiments** (new prompt, new skills, memory fixes) without affecting production, the system supports a **time-point sandbox** built on MatrixOne **Git for Data** (or equivalent branch/snapshot):

**Idea**:

1. **Snapshot at T1**: Create a **branch** or **clone** of the database (or key tables) as of a timestamp T1 (e.g. “production state at 2024-06-01 14:30:00”). MatrixOne Git for Data provides branch/commit semantics; alternatively, **clone** tables at T1.
2. **Sandbox = isolated branch**: All reads/writes in the sandbox hit the branch, not production. In the sandbox one can:
   - **Modify** prompt_templates, skills_registry, configs (e.g. new prompt, new skill).
   - **Replay** historical conversations (by session_id or causal_chain_id) against this modified state and observe new outputs.
   - **Compare** sandbox outputs vs original events to evaluate impact (A/B, what-if).
3. **Memory state at T1**: To make replay faithful, the **vector store** state at T1 should be restorable. **sessions.vector_db_snapshot_id** stores a ref (e.g. snapshot id from the vector DB or a replay anchor). When **starting a sandbox**, reconstruct vector state as follows:
   - **If the vector DB supports snapshots** (e.g. Pinecone): load the snapshot identified by `vector_db_snapshot_id`.
   - **If it does not**: **replay** events into the vector store from **memory_index_queue** (or from conversation_events ordered by created_at) up to the session’s time window; requires that indexing timestamps or event order are recorded so replay is well-defined.
   - **Document**: Vector DB **selection should consider snapshot/replay capability** if sandbox replay is required. **Fallback**: Sandbox can run with “current” vector DB only (faster, but not a precise historical state); label such runs as “sandbox vector state is not a historical snapshot.”
4. **Sandbox evaluation before merge**: Define **quantitative criteria** for sandbox runs: e.g. response quality (vs original events), safety intercept rate (if adversarial samples were injected), token efficiency. Produce an **evaluation report**; only then decide whether to merge (e.g. new prompt version) or discard.
5. **Causal-chain regression gate (recommended)**: Before merging a new prompt/skills version, **automatically** replay in sandbox the **last N low-score** and **last N high-score** causal chains with the candidate config; compute **quality delta**. If regression exceeds threshold (e.g. score drop on high-score chains, or no gain on low-score chains), **reject merge**. This gates production on data-driven regression checks rather than manual sampling.
6. **Merge or discard**: If sandbox outcomes meet the bar (and regression gate passes), **merge** changes back to main; if not, discard the branch.

**Production-grade practices**:
- **IaC (Infrastructure as Code)**: Sandbox creation and A/B runs can be driven by Git for Data branches and config-as-code (e.g. declare "replay N chains with template v2" in a job spec), so experiments are reproducible and auditable.
- **Versioning**: Use **semantic versioning (SemVer)** for prompt_templates and skills_registry versions where possible; supports drift detection and "pin exact version" for replay.
- **Safety / red-team**: Optionally run **prompt-injection or adversarial** tests in sandbox (inject attack samples, measure safety intercept rate); include in the sandbox evaluation report before merge. Reproducibility applies to the full stack (config, model, and if applicable numerical/scheduling assumptions).

**Use cases**:

| Scenario | Value |
|----------|--------|
| **Offline A/B** | Test new prompt/skills on 10k historical chains in sandbox; zero impact on live users; quantify gain/regression before release. |
| **Prompt evolution experiment** | Run genetic/LLM-based search (e.g. DSPy, TextGrad) on high/low-score chains in sandbox to generate candidate prompts; auto-evaluate and recommend merge → semi-automate “human review prompt”. |
| **Regression gate** | Before merge: auto-replay N low-score + N high-score chains with candidate version; reject merge if quality delta exceeds threshold. |
| **What-if / incident复盘** | “If we had used the new prompt at that moment…” → sandbox at T1, apply fix, replay, verify. |
| **Compliance / safety drill** | Inject sensitive or adversarial queries in sandbox; verify safety and policy responses. |
| **Memory correction** | Fix wrong knowledge in sandbox (e.g. update memory_chunks or vector store); replay affected conversations; confirm impact; then apply fix to production. |

Implementation depends on MatrixOne’s **Git for Data** (or clone) API. If Git for Data is not yet available, a **fallback** is **table clone + time-point query**: e.g. `CREATE TABLE sandbox_events AS SELECT * FROM conversation_events AS OF TIMESTAMP 'T1'` (syntax depends on MatrixOne), then run sandbox logic against the clone. The design assumes **branch-by-time** or **clone-by-time** and **isolated writes** in the sandbox so that production data is never modified by experiments.

> For detailed Git for Data implementation plans, SQL examples, industry reference analysis, and phased adoption roadmap, see [git-for-data-enhancements.md](./git-for-data-enhancements.md).

---

## 4. Table Design (MatrixOne)

All tables in MatrixOne. Schema is **event-centric**: one unified event table (**conversation_events**) plus **versioned configs** and **evaluation/training** tables so that history is analyzable, reproducible, and trainable.

### 4.1 Tables Overview

| Table                     | Purpose |
|---------------------------|--------|
| **conversation_events**   | Atomic events (user_query, llm_request, llm_response, tool_call, tool_result, system_message, **multi_agent_message**); user_id + session_id; context_snapshot, prompt_template_id, skills_snapshot; quality_score, training_eligible; embedding_ref; optional desensitized_content. |
| **sessions**              | Conversation scope; identity, lifecycle, last_event_id, event_count, last_active_at, summary_status. |
| **prompt_templates**      | Versioned prompt templates (template_id, version, content, effective_at, is_active). |
| **skills_registry**       | Versioned skill definitions (skill_id, version, schema, description). |
| **agent_configs**         | Agent-level config snapshots (agent_id, version, config_json). |
| **configs**               | Key-value config (budgets, session limits, feature flags). Scoped by scope_type/scope_id. |
| **event_evaluations**     | User/system evaluations per event (event_id, score, dimensions, source: user_feedback \| auto_metric \| human_label). |
| **training_annotations**   | Training labels (event_id, label, dataset_split, exported_at). |
| **data_export_jobs**      | Export pipeline (filters_json, format, status, file_ref). |
| **tokens**                | Secrets: type, scope, provider, secret_ref or encrypted_value; is_active, rotation_policy. |
| **repos**                 | Registered repos; token_id, normalization_status, last_synced_at, webhook_secret_ref. |
| **token_usage_log**       | Audit: token_id, used_at, success, error_code. Async write. |
| **token_rotation_jobs**   | Scheduled rotation. |
| **memory_index_queue**    | RAG indexing queue: **event_id**, status, retry_count. |
| **session_summaries**     | Medium-term memory: summary_text, source_event_ids, summarizer_model/params, token_count. Reproducible summaries. |
Optional later: `session_summaries`, `data_retention_policy` (for partitioning/archival).

### 4.2 Column Definitions (Evolvable)

**conversation_events** (replaces turns; event-centric, full traceability)

| Column               | Type     | Purpose |
|----------------------|----------|--------|
| event_id             | PK (e.g. ULID) | Global unique event id; sortable. |
| user_id              | string   | **Core 串联 key**; every event has user_id for cross-session/user queries. |
| session_id           | string   | Session this event belongs to. |
| agent_id             | string   | Agent type (e.g. dev-agent, chat-agent). |
| agent_version        | string   | Agent code/config version. |
| event_type           | string   | user_query \| llm_request \| llm_response \| tool_call \| tool_result \| system_message \| **multi_agent_message** (cross-agent message, e.g. AutoGen-style group chat; causal_chain extends across agents). |
| content              | text     | Original content (for reproducibility and training). 脱敏 optional later. |
| **desensitized_content** | text     | Nullable. Optional **compliance-ready** field: at write time, optionally fill with a desensitized version of content. When compliance is required later, only **write logic** changes; no schema migration. Default NULL. |
| metadata             | JSON     | **Namespace convention**: e.g. dev.code_path, chat.topic; business-specific fields isolated. |
| context_snapshot     | JSON     | **Reproducibility**: prompt_template_id, skills_used (id+version), history_events (event_ids), retrieved_chunks (chunk_ids). **Size**: constrain in app or via DB CHECK to avoid oversized JSON; prefer refs + minimal snippets over full prompt text. |
| token_usage          | JSON     | e.g. {"prompt":1200, "completion":300, "total":1500}. |
| embedding_ref        | varchar  | External vector store chunk id; MatrixOne stores ref only. |
| created_at           | timestamp | |
| prompt_template_id   | string   | References prompt_templates (e.g. template_id+version). |
| skills_snapshot      | JSON     | e.g. [{"id":"review", "version":"v2", "used":true}]. |
| quality_score        | decimal(3,2) | System pre-score (0–5). Nullable. |
| is_flagged           | bool     | Default false. |
| training_eligible    | bool     | Default false; set by rule or evaluation for training pipeline. |
| **parent_event_id**  | string   | Nullable; immediate prior event in causal chain (e.g. llm_response → llm_request). |
| **causal_chain_id**  | string   | e.g. ULID; groups one user query + full LLM/tool chain for replay. |
| **llm_model_used**   | string   | Nullable; e.g. gpt-4o-2024-05, qwen-max. |
| **llm_params**       | JSON     | Nullable; e.g. {"temperature":0.7, "max_tokens":1024}. |

Indexes: `(user_id, created_at)`, `(session_id, created_at DESC)`, `(training_eligible, quality_score DESC)`, **`(causal_chain_id, created_at)`** (replay by chain).

**sessions**

| Column          | Type     | Purpose |
|-----------------|----------|--------|
| session_id      | PK (UUID) | |
| user_id         | string   | Required |
| tenant_id       | string   | Nullable |
| created_at      | timestamp | |
| updated_at      | timestamp | |
| last_active_at  | timestamp | Recovery, idle |
| status          | string   | active, idle, closed |
| last_event_id   | string (ref to conversation_events) | Nullable; **no FK** to avoid circular dependency (sessions ↔ events); **application-maintained**, e.g. updated asynchronously by the event write service. |
| event_count     | int      | Default 0; cap enforcement |
| summary_status       | string   | Nullable: pending, completed, failed |
| summary_job_id       | string   | Nullable |
| **vector_db_snapshot_id** | string   | Nullable; ref to external vector DB snapshot at session time (for replay/sandbox). |
| metadata             | JSON     | Nullable |

**prompt_templates** (versioned; anti-fragility)

| Column       | Type     | Purpose |
|--------------|----------|--------|
| template_id  | string   | Logical template id. |
| version      | string   | Version tag (e.g. v1, 20240101). |
| content      | text     | Template body (Markdown, placeholders). |
| effective_at | timestamp | When this version became effective. |
| is_active    | bool     | Currently active for new requests. |
| created_at   | timestamp | |

Composite PK (template_id, version) or single PK. Events store prompt_template_id (e.g. template_id@version) so history always binds to the config that was used.

**skills_registry** (versioned)

| Column      | Type     | Purpose |
|-------------|----------|--------|
| skill_id    | string   | |
| version     | string   | |
| schema      | JSON     | Params schema, tool schema. |
| description | text     | Short description for prompt. |
| created_at  | timestamp | |

**agent_configs** (versioned snapshots)

| Column      | Type     | Purpose |
|-------------|----------|--------|
| agent_id    | string   | |
| version     | string   | |
| config_json | JSON     | Full agent config snapshot. |
| created_at  | timestamp | |

**event_evaluations** (evaluation → training pipeline)

| Column       | Type     | Purpose |
|--------------|----------|--------|
| eval_id      | PK (UUID) | |
| event_id     | FK conversation_events | |
| evaluator_id | string   | user or system. |
| score        | decimal(3,2) | Overall score. |
| dimensions   | JSON     | e.g. {"relevance":5, "helpfulness":4, "safety":5}. |
| feedback     | text     | Optional. |
| source       | string   | user_feedback \| auto_metric \| human_label. |
| created_at   | timestamp | |

Index: (event_id).

**training_annotations** (labels for export)

| Column        | Type     | Purpose |
|---------------|----------|--------|
| annotation_id | PK (UUID) | |
| event_id      | FK conversation_events | |
| label         | string   | e.g. high_quality, edge_case, needs_correction. |
| reason        | text     | Optional. |
| dataset_split | string   | train \| val \| test. |
| exported_at   | timestamp | When exported to dataset. |

Index: (label, dataset_split).

**data_export_jobs** (safe export for training)

| Column       | Type     | Purpose |
|--------------|----------|--------|
| export_id    | PK (UUID) | |
| filters_json | JSON     | e.g. training_eligible=true, quality_score>=4. |
| format       | string   | jsonl, parquet. |
| status       | string   | pending, running, completed, failed. |
| file_ref     | string   | Path or object ref to exported file. |
| created_at   | timestamp | |

**configs**

| Column     | Type     | Purpose |
|------------|----------|--------|
| scope_type | string   | global, tenant, user |
| scope_id   | string   | Nullable (e.g. tenant_id, user_id) |
| key        | string   | e.g. prompt_template_v1, context_max_tokens, session_idle_timeout_hours, max_turns_per_session |
| value      | text/JSON | |
| updated_at | timestamp | |

Composite PK or config_id PK as preferred.

**tokens**

| Column           | Type     | Purpose |
|------------------|----------|--------|
| token_id         | PK (UUID) | |
| type             | string   | repo, llm |
| provider         | string   | e.g. github, openai, groq |
| scope_user_id    | string   | Nullable |
| scope_tenant_id  | string   | Nullable |
| scope_repo       | string   | Nullable |
| secret_ref       | string   | Preferred: Vault path or secret manager ref |
| encrypted_value  | text     | Alternative if no secret manager |
| created_at       | timestamp | |
| expires_at       | timestamp | Nullable |
| is_active        | bool     | Default true; set false on 401 etc. |
| rotation_policy  | string   | Nullable; e.g. manual, 90d |
| metadata         | JSON     | Nullable |

**repos**

| Column               | Type     | Purpose |
|----------------------|----------|--------|
| repo_id              | PK (UUID) | |
| repo_url             | string   | Or (owner_id, repo_name) |
| owner_id             | string   | |
| token_id             | FK tokens | Nullable |
| normalization_status | string   | pending, done, failed |
| metadata             | JSON     | Paths, etc. |
| last_synced_at       | timestamp | Nullable; sync monitoring |
| webhook_secret_ref   | string   | Nullable; for event verification |
| created_at, updated_at | timestamp | |

**token_usage_log** (async write, audit)

| Column    | Type     | Purpose |
|-----------|----------|--------|
| log_id    | PK (UUID) | |
| token_id  | FK tokens | |
| used_at   | timestamp | |
| success   | bool     | |
| error_code| int      | Nullable; 401 → alert / deactivate |

**token_rotation_jobs**

| Column       | Type     | Purpose |
|--------------|----------|--------|
| job_id       | PK (UUID) | |
| token_id     | FK tokens | |
| scheduled_at | timestamp | |
| status       | string   | pending, completed, failed |
| created_at   | timestamp | |

**memory_index_queue**

| Column     | Type     | Purpose |
|------------|----------|--------|
| queue_id   | PK (UUID) | |
| event_id   | ref conversation_events | |
| status     | string   | pending, processing, completed, failed |
| retry_count| int      | Default 0 |
| created_at | timestamp | |

### 4.3 Metadata Namespace

- **metadata** in conversation_events uses a **namespace convention** to keep business domains decoupled: e.g. `dev.code_path`, `dev.repo`, `chat.topic`. New business fields are added under a domain prefix without changing core schema; optional **business annotation** tables can reference event_id for richer domain data.
- **脱敏 (desensitization)**: Not required for MVP. Content is stored as **original** to maximize replay accuracy and training utility. If compliance requires 脱敏 later, it can be added as a separate pipeline (e.g. export-time or a dedicated 脱敏 service before write) with configurable strength; the design does not assume 脱敏 by default.

### 4.4 Indexes

- **conversation_events**: `(user_id, created_at)`, `(session_id, created_at DESC)`, `(training_eligible, quality_score DESC)`, **`(causal_chain_id, created_at)`** (replay by chain).
- **sessions**: `(user_id, status, last_active_at DESC)`.
- **tokens**: `(scope_user_id, type)`, `(scope_tenant_id, type)`.
- **repos**: `(owner_id)`, `(repo_url)` or unique `(owner_id, repo_name)`.
- **event_evaluations**: `(event_id)`.
- **training_annotations**: `(label, dataset_split)`.

### 4.5 CREATE Statements (Example Sketches)

```sql
-- Conversation events (event-centric; full traceability)
CREATE TABLE conversation_events (
  event_id            VARCHAR(64) PRIMARY KEY,
  user_id             VARCHAR(64) NOT NULL,
  session_id          VARCHAR(64) NOT NULL,
  agent_id            VARCHAR(64) NOT NULL,
  agent_version       VARCHAR(32) NOT NULL,
  event_type          VARCHAR(24) NOT NULL CHECK (event_type IN (
    'user_query', 'llm_request', 'llm_response', 'tool_call', 'tool_result', 'system_message', 'multi_agent_message'
  )),
  content             TEXT NOT NULL,
  desensitized_content TEXT,
  metadata            JSON,
  context_snapshot    JSON,
  token_usage         JSON,
  embedding_ref       VARCHAR(128),
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  prompt_template_id  VARCHAR(64),
  skills_snapshot     JSON,
  quality_score      DECIMAL(3,2),
  is_flagged          BOOLEAN DEFAULT FALSE,
  training_eligible   BOOLEAN DEFAULT FALSE,
  parent_event_id     VARCHAR(64),
  causal_chain_id     VARCHAR(64),
  llm_model_used     VARCHAR(50),
  llm_params         JSON
);
CREATE INDEX idx_ce_user_time ON conversation_events(user_id, created_at);
CREATE INDEX idx_ce_session ON conversation_events(session_id, created_at DESC);
CREATE INDEX idx_ce_training ON conversation_events(training_eligible, quality_score DESC);
CREATE INDEX idx_causal_chain ON conversation_events(causal_chain_id, created_at);

-- Sessions (last_event_id: app-maintained ref, no FK)
CREATE TABLE sessions (
  session_id     VARCHAR(36) PRIMARY KEY,
  user_id        VARCHAR(255) NOT NULL,
  tenant_id      VARCHAR(255),
  created_at     TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at     TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  last_active_at TIMESTAMP,
  status         VARCHAR(32),
  last_event_id  VARCHAR(64),
  event_count   INT DEFAULT 0,
  summary_status VARCHAR(32),
  summary_job_id VARCHAR(255),
  vector_db_snapshot_id VARCHAR(128),
  metadata       JSON
);
CREATE INDEX idx_sessions_user_status_active ON sessions(user_id, status, last_active_at DESC);

-- Prompt templates (versioned)
CREATE TABLE prompt_templates (
  template_id   VARCHAR(64) NOT NULL,
  version       VARCHAR(32) NOT NULL,
  content       TEXT NOT NULL,
  effective_at  TIMESTAMP,
  is_active     BOOLEAN DEFAULT TRUE,
  created_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (template_id, version)
);

-- Event evaluations
CREATE TABLE event_evaluations (
  eval_id      VARCHAR(64) PRIMARY KEY,
  event_id     VARCHAR(64) NOT NULL,
  evaluator_id VARCHAR(64),
  score        DECIMAL(3,2),
  dimensions   JSON,
  feedback     TEXT,
  source       VARCHAR(20),
  created_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_eval_event ON event_evaluations(event_id);

-- Training annotations
CREATE TABLE training_annotations (
  annotation_id VARCHAR(64) PRIMARY KEY,
  event_id      VARCHAR(64) NOT NULL,
  label         VARCHAR(50),
  reason        TEXT,
  dataset_split VARCHAR(20),
  exported_at   TIMESTAMP
);
CREATE INDEX idx_ta_label_split ON training_annotations(label, dataset_split);

-- Memory index queue (RAG; event_id)
CREATE TABLE memory_index_queue (
  queue_id    VARCHAR(36) PRIMARY KEY,
  event_id    VARCHAR(64) NOT NULL,
  status      VARCHAR(32) NOT NULL,
  retry_count INT DEFAULT 0,
  created_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Regression gate results
CREATE TABLE gate_results (
  gate_id         VARCHAR(64) PRIMARY KEY,
  change_type     VARCHAR(50) NOT NULL,   -- 'skill_change' | 'prompt_change' | 'config_change'
  change_id       VARCHAR(255) NOT NULL,  -- e.g. 'summarize_pr@2.0.0'
  snapshot_used   VARCHAR(255) NOT NULL,  -- Git for Data snapshot name
  sessions_tested INT NOT NULL,
  error_rate      DECIMAL(5,4),
  passed          BOOLEAN NOT NULL,
  metrics         JSON,                   -- detailed metrics breakdown
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_change (change_type, change_id),
  INDEX idx_created (created_at)
);

-- Training datasets (versioned via snapshots)
CREATE TABLE training_datasets (
  dataset_id      VARCHAR(64) PRIMARY KEY,
  name            VARCHAR(255) NOT NULL,
  snapshot_name   VARCHAR(255) NOT NULL,  -- Git for Data snapshot = dataset version
  event_count     INT NOT NULL,
  pair_count      INT NOT NULL,           -- SFT instruction-response pairs
  criteria        JSON NOT NULL,          -- selection criteria used
  quality_stats   JSON,                   -- avg_score, score_distribution, etc.
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  created_by      VARCHAR(255),
  INDEX idx_name (name),
  INDEX idx_snapshot (snapshot_name)
);

-- Hallucination verification log
CREATE TABLE hallucination_checks (
  check_id        VARCHAR(64) PRIMARY KEY,
  event_id        VARCHAR(64) NOT NULL,   -- LLM response event
  session_id      VARCHAR(64) NOT NULL,
  snapshot_used   VARCHAR(255),           -- snapshot used for verification
  claims_total    INT NOT NULL,
  claims_verified INT NOT NULL,
  claims_contradicted INT NOT NULL,
  claims_unverifiable INT NOT NULL,
  safe_to_deliver BOOLEAN NOT NULL,
  contradictions  JSON,                   -- details of contradicted claims
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_event (event_id),
  INDEX idx_session (session_id),
  INDEX idx_safe (safe_to_deliver)
);

-- Prompt experiment branches
CREATE TABLE prompt_experiments (
  experiment_id   VARCHAR(64) PRIMARY KEY,
  template_id     VARCHAR(64) NOT NULL,
  branch_name     VARCHAR(255) NOT NULL,  -- Git for Data branch/snapshot
  hypothesis      TEXT,
  candidate_content TEXT NOT NULL,
  baseline_metrics JSON,
  experiment_metrics JSON,
  quality_delta   DECIMAL(5,4),
  status          VARCHAR(50) DEFAULT 'running', -- 'running' | 'passed' | 'failed' | 'merged'
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  completed_at    TIMESTAMP,
  INDEX idx_template (template_id),
  INDEX idx_status (status)
);
```

(Tokens, token_usage_log, token_rotation_jobs, configs, repos, data_export_jobs, skills_registry, agent_configs: same pattern as in 4.2; adjust to MatrixOne SQL dialect.)

### 4.6 Analytics, Retention, and Export

- **Pre-aggregated views**: e.g. user_daily_stats (user_id, date, event_count, …) as a **materialized view** or **scheduled rollup table** for fast analytics. MatrixOne supports views; materialized views or rollups depend on product support.
- **Retention and archival**: **MatrixOne currently does not support table partitioning.** Use **application-level retention**: e.g. **data_retention_policy** config (or table) defining hot/warm/cold and retention (e.g. 10 years); a scheduled job **archives or moves** old rows to a cold table (e.g. conversation_events_archive) or external storage, or deletes per policy. Design so that when MatrixOne gains partitioning support, migration is straightforward (same schema, add partition key). **Monitor write hotspot** (e.g. by session_id or user_id) and document a **sharding/split strategy** (e.g. by user_id hash or tenant_id) for application-level split if a single table becomes a bottleneck.
- **Export pipeline**: **data_export_jobs** table; job consumes filters (e.g. training_eligible=true, quality_score>=4), exports to JSONL/Parquet, writes file_ref; optionally triggers downstream training pipeline. Access control and audit on export.

### 4.7 Evaluation → Training Loop (Closed Loop)

1. **Evaluate**: User feedback (thumbs up/down) or **auto_metric** (e.g. task success, latency) or **human_label** → write **event_evaluations** (source: user_feedback | auto_metric | human_label).
2. **Eligibility**: Rule engine or batch job sets **conversation_events.training_eligible** (e.g. score >= threshold, not is_flagged); **quality_score** can be written from event_evaluations or heuristic.
3. **Annotate**: Human review (optional) or rule-based labels → **training_annotations** (label, dataset_split).
4. **Export**: **data_export_jobs** with filters on training_eligible/label/split → produce JSONL/Parquet for SFT or RLHF. **Access control and audit** on export (already in §4.6); for sensitive data, support **encryption at rest** for export artifacts. **training_annotations** can be extended with **preference pairs** (e.g. chosen_event_id, rejected_event_id) for RLHF preference datasets.
5. **Model iteration**: Training pipeline consumes export; new model/agent version; next events reference new agent_version. Closed loop.

**Automation and innovation**:
- **Early auto_metric**: Implement a **lightweight auto_metric** early (e.g. a small model or rule set that answers “task completed?” or “obvious hallucination?”). Write to **event_evaluations** with source=auto_metric so **training_eligible** can be populated without waiting for user feedback; reduces dependence on manual labels and speeds up the training pipeline.
- **Online fine-tuning trigger (innovation)**: When **training_eligible** events accumulate beyond a threshold (e.g. per user or per tenant), the system can **trigger small-scale online LoRA fine-tuning** (or similar) targeted at that user/tenant. After training, **validate in sandbox** (replay sample chains with the new adapter) before enabling in production. This enables “**agent evolves with user interaction**” in near real-time, while keeping risk bounded by sandbox validation.

### 4.8 Design Openness

- Git for data can be applied later to prompt_templates, skills_registry, and selected tables.
- RBAC/row-level: filter by user_id/tenant_id; all event and session tables carry these where needed.
- Event write hotspot: index (session_id, created_at DESC). MatrixOne does not support partitioning; use app-level retention and sharding/split预案 (see §4.6 and §8).

---

## 5. Token Management

**Tokens** are secrets for repo and LLM APIs. **Storage**: prefer **secret_ref** (Vault or cloud secret manager); **encrypted_value** in MatrixOne only as fallback. **Never log or expose plain values.**

### 5.1 Resolution Priority (Written Order)

**Repo token** (for a given request with user_id, tenant_id, repo_url or repo_id):

1. **Repo-specific**: If repo_url/repo_id given, look up `repos` → token_id; load token, verify scope matches request user/tenant; if active, return.
2. **User default**: tokens where type=repo, scope_user_id=user_id, scope_repo IS NULL, is_active=true. Prefer one with provider matching repo host if available.
3. **Tenant default**: tokens where type=repo, scope_tenant_id=tenant_id, scope_repo IS NULL, is_active=true.
4. **Global fallback**: only if config allows (e.g. single-tenant); type=repo, scope_user_id and scope_tenant_id both NULL.

**LLM token**: Same idea: user-scoped → tenant-scoped → global (if allowed). Use `provider` to choose among multiple LLM keys (e.g. openai vs groq) when needed.

### 5.2 Storage and Security

- **Prefer secret_ref**: Store Vault path or secret manager key in `tokens.secret_ref`; app resolves at runtime. **Encrypted_value** only when no secret manager is available; key from env, never in DB.
- **Access**: Only the component that needs the token (GitHub client, LLM client) receives it; context builder and logs **never** see raw values.
- **Audit**: Write to **token_usage_log** on each use (used_at, success, error_code); **never log the token value**. On **401** (or configurable error codes), trigger alert and/or set **tokens.is_active = false** so resolution skips it until rotated.

### 5.3 Rotation and Expiry

- **expires_at**: If set, token service does not return expired tokens; alert or enqueue rotation job.
- **token_rotation_jobs**: Table supports scheduled rotation (token_id, scheduled_at, status). Cron or job runner creates/updates jobs; after rotation, update `tokens` (new secret_ref or encrypted_value) and mark job completed.
- **First version**: Rotation can be manual; document flow. Automation uses token_rotation_jobs in a later phase.

### 5.4 Token Resolution Flow (Diagram)

```
Resolve repo_token(user_id, tenant_id, repo_url?):
  if repo_url:
    repo = repos.find(repo_url, owner=user_id|tenant_id)
    if repo.token_id: token = tokens.get(repo.token_id); if token.is_active return token
  token = tokens.find(type=repo, scope_user_id=user_id, scope_repo=null, is_active=true)
  if token: return token
  token = tokens.find(type=repo, scope_tenant_id=tenant_id, scope_repo=null, is_active=true)
  if token: return token
  if config.allow_global_repo_token: return tokens.find(type=repo, scope_*=null)
  return null
```

---

## 6. Observability (MVP-Ready)

- **conversation_events.metadata** and **token_usage**: Reserve a **metrics** object in metadata or use token_usage, e.g. `{"prompt":1200, "completion":300}` and `context_build_ms`, `retrieval_ms`, `retrieval_hit`. Enables analysis without new tables.
- **Key metrics**: Context assembly duration, total context tokens, retrieval latency and hit rate, session active count. Export to **Prometheus** (e.g. `mo_agent_context_token_usage`, `mo_agent_session_active_count`) for dashboards and alerting.
- **Performance hotspot monitoring**: Monitor **write rate** and **query latency** by session_id and user_id (e.g. top-N hot sessions); feed into **sharding/split预案** when a single table or index becomes a bottleneck (MatrixOne has no partitioning; app-level split or archive is the lever).
- **Alerts**: Context over-budget (truncation), token 401 (token_usage_log), RAG timeout rate.
- **Success metrics (targets)**: Reproducibility rate (replay matches original when same config/model) &gt;99%; context utilization (fraction of budget used) &lt;80% typical to leave headroom; training loop cycle (evaluate → export → train) &lt;1 week when automated; orphan event rate (events with no parent and not user_query) &lt;1%. Review sandbox experiment coverage periodically.

---

## 7. Test Anchors

- **Context assembly**: Unit test that when total tokens exceed budget, “current task” and (if any) “retrieved memory” are retained and “recent conversation” is truncated; verify no over-budget prompt; verify **context_snapshot** is persisted with prompt_template_id and history_events.
- **Token resolution**: Parameterized tests over all scope fallback paths (repo-specific → user → tenant → global); verify correct token returned and 401 path sets is_active=false.
- **Session lifecycle**: Integration test: create session → append events → “get last N events” (by session_id or user_id) → enforce max_events_per_session (archive or reject) → GET /sessions/latest returns expected session.
- **Reproducibility**: Given an event_id, load event + context_snapshot; resolve prompt_template and skills by version; reconstruct context and verify it matches the snapshot (or document the repro API).
- **Replay verification (backtrace)**: **Scenario — Reproduce a user-reported wrong answer**: Given user_id "U123" and a timestamp (e.g. 2024-06-01 14:30:22) of the problematic reply; When load the event at that time and its causal_chain_id; And reconstruct input from context_snapshot + prompt_template_id@version; And call the same model (llm_model_used) with same params (llm_params); Then the generated response should **match** the original llm_response.content (bit-for-bit or semantically), proving full reproducibility. If it does not, the cause (template drift, missing context, model non-determinism) can be isolated.

---

## 8. Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Context over token limit → LLM failure | Token Budget Manager enforces truncation; log warning; return user message (“Conversation shortened…”). |
| tokens table as attack surface | DB encryption for sensitive columns; app least privilege; audit via token_usage_log; no plain value in logs. |
| conversation_events write hotspot | Index on (session_id, created_at). **MatrixOne does not support partitioning** today: use **app-level retention** (archive/delete by policy) and **monitor** write rate by session_id/user_id; have a **sharding/split预案**: e.g. app-level split by user_id or tenant_id (multiple tables or logical shards), or async batch insert when supported. |
| RAG latency spikes | Retrieval timeout (e.g. 800ms); on timeout, omit “Retrieved memory” and record metric; degrade gracefully. |
| **Causal chain break** | Same causal_chain_id events written in order (transaction or strict ordering); **monitor orphan events** (no parent and not user_query) and alert. |
| **Sandbox vector rebuild cost** | **Degradation**: sandbox can optionally “use current vector DB only” (faster, not historically exact); document “sandbox vector state is not a historical snapshot” when so. |
| **MatrixOne Git for Data not ready** | **Fallback**: table clone + time-point query (e.g. `SELECT * FROM conversation_events AS OF TIMESTAMP 'T1'`) to build sandbox tables; then run sandbox logic against the clone. |

---

## 9. Implementation Roadmap (Phased)

| Phase | Focus |
|-------|--------|
| **Phase 0 (Foundation)** | Create tables: **conversation_events**, sessions, **prompt_templates**, **skills_registry**, configs, tokens, repos, token_usage_log, token_rotation_jobs, memory_index_queue (event_id); token storage and resolution; basic CRUD. **Seed fields from day one** (do not add later): **conversation_events.causal_chain_id**, **conversation_events.desensitized_content** (nullable, compliance-ready), **sessions.vector_db_snapshot_id**, **prompt_templates.version**—so that historical data can support replay and sandbox without migration, and compliance can be enabled by write logic only. |
| **Phase 1 (MVP core)** | Session create/append; **event** persistence (event_type, context_snapshot, token_usage, prompt_template_id, skills_snapshot, **causal_chain_id**, parent_event_id, llm_model_used, llm_params); context assembly with Token Budget Manager and versioned prompt_templates; LLM call. Content stored as **original** (脱敏 optional later). |
| **Phase 2 (Observability + Evaluation)** | Metrics in conversation_events.metadata/token_usage; Prometheus; **event_evaluations** (user thumbs up/down, auto_metric); **training_eligible** and **quality_score** rules or batch jobs. |
| **Phase 3 (Intelligence + Training loop)** | Async session summary; RAG pipeline (memory_index_queue by event_id, external vector store, embedding_ref); **training_annotations** and **data_export_jobs**; export pipeline for SFT/RLHF. |
| **Phase 4 (Experience + Analytics)** | GET /sessions/latest; dynamic skill filtering; max_events_per_session enforcement; **agent_configs** versioning; pre-aggregated views or rollups; retention policy and partitioning. |
| **Phase 5 (Replay + Sandbox)** | **Causal chain** (parent_event_id, causal_chain_id, llm_model_used, llm_params); **Replay API** (replay_session / replay_chain, optional target_llm); **Time-point sandbox** (MatrixOne Git for Data branch/clone at T1, sandbox replay, vector_db_snapshot_id); automation for sandbox evaluation and merge. |

---

## 10. Summary: First-Step Deliverables

| Area | Deliverable |
|------|-------------|
| **Event-centric model** | **conversation_events** (event_id, user_id, session_id, agent_id, event_type including **multi_agent_message**, context_snapshot, prompt_template_id, skills_snapshot, quality_score, training_eligible, causal_chain_id, parent_event_id, llm_*, optional **desensitized_content**); **user_id** as global 串联 key; **content** = original (脱敏 optional); **metadata** namespace convention (e.g. dev.*, chat.*). |
| Context | Section layout; **Token Budget Manager**; **versioned prompt_templates** and **skills_registry**; **context_snapshot** persisted per LLM call for 100% reproducibility; optional dynamic skill filtering; RAG timeout and fallback. |
| Memory | Short-term = conversation_events (by session or user); medium-term = summary_status/job; long-term = **embedding_ref** + external vector store, memory_index_queue (event_id). Evolution: **hierarchical retrieval** (HyDE, multi-vector, parent-child chunking); **adaptive compression** (summarization when over budget, recorded as event for audit). |
| Session | Identity + last_active_at, **last_event_id**, **event_count**; lifecycle (create, append, idle, max_events); **GET /sessions/latest**; config: session_idle_timeout_hours, max_events_per_session. |
| Versioned configs | **prompt_templates**, **skills_registry**, **agent_configs** (template_id+version, skill_id+version); events reference these for **抗变化** and traceability. |
| Evaluation & training | **event_evaluations** (score, dimensions, source: user_feedback | **auto_metric** | human_label); **training_annotations** (label, dataset_split); **training_eligible** on events; **data_export_jobs**; closed loop. **Early auto_metric** (e.g. task-complete/hallucination) to automate training_eligible; **online LoRA trigger** (per user/tenant after threshold) with sandbox validation. |
| Tables | conversation_events, sessions, prompt_templates, skills_registry, agent_configs, configs, event_evaluations, training_annotations, data_export_jobs, tokens, repos, token_usage_log, token_rotation_jobs, memory_index_queue; CREATE examples; analytics (views, retention, export). |
| Tokens | **secret_ref preferred**; resolution order (repo-specific → user → tenant → global); **token_usage_log** and 401 → is_active=false; token_rotation_jobs. |
| Observability | conversation_events.metadata and token_usage; Prometheus; test anchors (including reproducibility from context_snapshot) and risk table. |
| **Replay and causal chain** | **parent_event_id**, **causal_chain_id**, **llm_model_used**, **llm_params** on conversation_events; index (causal_chain_id, created_at); Replay workflow (load chain → reconstruct from context_snapshot → re-run same or new model → compare). |
| **Time-point sandbox** | MatrixOne Git for Data branch/clone at T1; sandbox = isolated branch (modify prompt/skills, replay history); **vector_db_snapshot_id** on sessions for memory state at T1. **Prompt evolution experiments** (genetic/LLM search in sandbox → candidate versions → auto-evaluate → recommend merge); **regression gate** (replay N low/high-score chains before merge; reject if delta exceeds threshold); merge or discard. |
| **Memory–Prompt–Context** | Three-layer model (Memory / Prompt / Context) defined; **post_conversation_hook** (scoring → training_eligible, knowledge extraction, prompt signals). |

Implementation can proceed Phase 0 → 1 → … → 5; each phase builds on the previous. The design treats conversation as **traceable, analyzable, trainable data assets** with MatrixOne as the single persistence layer and vectors only as refs.

**Three “operating-system-level” capabilities** (Phase 5 and ongoing):

- **Conversation replay (“对话时光机”)**: Causal chain (parent_event_id, causal_chain_id) and LLM params (llm_model_used, llm_params) enable exact **replay** of any past chain; optional **replay with a different model** for regression testing and training data. Shifts debugging from “infer from logs” to “re-run and compare”.
- **Time-point sandbox (“平行宇宙实验台”)**: MatrixOne Git for Data (or clone) provides **branch-at-T1** and **isolated sandbox**; new prompt/skills/memory can be tested on historical traffic with **zero production impact**; merge or discard after evaluation.
- **Memory–Prompt–Context clarity**: Explicit three-layer model (Memory = persistent knowledge, Prompt = versioned behavior, Context = one-shot assembled input + snapshot) keeps storage, versioning, and replay semantics consistent and programmable.


---

## 11. From Design to Engineering: Operational Completeness

**Context**: This section addresses the review feedback that "操作性完备" (operational completeness) is currently at the design vision layer, lacking concrete validation and acceptance paths. The goal is to bridge the gap between design and provable operational capability.

### 11.1 Engineering Validation Roadmap

The following capabilities move from "design points" to **engineering-validated features** with measurable acceptance criteria:

| Capability | Design Status | Engineering Target | Validation Method |
|------------|---------------|-------------------|-------------------|
| **Replay with quality gate** | Designed (§10, Phase 5) | Automated replay gate with 6 metrics | CI/CD integration, 95% pass rate |
| **Sandbox-based validation** | Designed (§10, Phase 5) | Sandbox lifecycle + isolation guarantees | End-to-end test, zero cross-contamination |
| **Prompt/Skill evolution** | Designed (§10, Phase 5) | Closed-loop automation (trigger → optimize → validate → deploy) | Weekly optimization runs, < 2 week cycle time |
| **Training data pipeline** | Designed (§10, Phase 3) | Automated export + quality filtering | Monthly training dataset generation |
| **A/B testing framework** | Not yet designed | Controlled rollout with statistical analysis | 2 A/B tests per quarter |

**Key insight**: The design provides the **data model and abstractions** (conversation_events, context_snapshot, versioned configs, sandbox); the engineering work is to build the **automation and quality gates** on top of these primitives.

### 11.2 Concrete Deliverables (Next 8 Weeks)

**Week 1-2: Replay Gate MVP**
- Deliverable: `mo-agent replay-gate run` command
- Acceptance: Runs 50 golden sessions in < 15 minutes, produces pass/fail decision
- Metrics: 6 automated metrics (success rate, output stability, latency, token efficiency, skill accuracy, error rate)
- Validation: Manual spot-check 5 sessions, false positive rate < 5%

**Week 3-4: Sandbox Validation**
- Deliverable: `mo-agent sandbox create/load/validate/delete` commands
- Acceptance: Sandbox creation < 30 seconds, zero production data affected
- Isolation: Separate DB, config, metrics namespace
- Validation: Audit query confirms no cross-contamination

**Week 5-6: Evolution Automation**
- Deliverable: `mo-agent evolution optimize-prompt` and `discover-skills` commands
- Acceptance: Generates valid candidate, runs replay gate automatically
- Trigger: Automated (satisfaction < 3.5, error rate > 5%) or manual
- Validation: Finds at least 1 skill gap in test data

**Week 7-8: CI/CD Integration**
- Deliverable: GitHub Actions workflow for replay gate on PR
- Acceptance: Gate runs in < 20 minutes, PR comment shows metrics
- Merge protection: Blocks merge if gate fails
- Validation: End-to-end test with real PR

### 11.3 Quality Metrics & Acceptance Criteria

**Operational metrics** (3 months post-deployment):
- Replay gate adoption: 100% of prompt/skill changes
- Gate pass rate: > 90%
- False positive rate: < 5%
- Sandbox usage: > 50 experiments/month
- Evolution cycle time: < 2 weeks (trigger → production)

**Quality metrics** (6 months post-deployment):
- User satisfaction: > 4.0/5 (sustained)
- Production error rate: < 2%
- Prompt optimization frequency: 1-2 per month
- New skills added: 3-5 per quarter

**Business metrics**:
- Reduced manual testing time: 80% (from 4 hours → 48 minutes per change)
- Faster iteration: 50% reduction in time-to-production for new features
- Increased confidence: 95% of changes deployed without rollback

### 11.4 Reference Implementation

See **[Replay, Sandbox, Evaluation & Evolution: Engineering Validation](replay-sandbox-evaluation-automation.md)** for complete specification including:
- Automated replay gating with golden session selection
- Sandbox lifecycle and isolation guarantees
- Skill/Prompt evolution closed-loop workflow
- A/B testing framework
- CI/CD integration patterns
- Risk mitigation strategies

**Key difference from this document**: This document defines the **data model and design principles**; the engineering validation document defines the **automation, workflows, and acceptance tests** that prove operational completeness.

### 11.5 Success Criteria for "Provably Leading"

To claim **"可证明的领先"** (provable leadership), the system must demonstrate:

1. **Reproducibility**: Any historical conversation can be replayed with bit-for-bit accuracy (or documented variance)
2. **Automation**: Prompt/skill changes go through automated quality gates without manual testing
3. **Isolation**: Experiments run in sandboxes with zero production impact
4. **Traceability**: Every production decision can be traced to specific prompt version, skills, and context
5. **Evolution**: Closed-loop improvement (feedback → optimization → validation → deployment) runs continuously

**Validation method**: Public demo showing:
- Replay of 6-month-old conversation with exact reproduction
- Automated replay gate blocking a regression
- Sandbox experiment with prompt optimization
- A/B test results driving production deployment
- Training dataset export from production events

**Timeline**: All 5 capabilities demonstrated by **Week 12** (3 months from start).

---

## 12. Conclusion

This design provides a **complete foundation** for event-centric, reproducible, evolvable conversation management:

- **Data model**: conversation_events, sessions, versioned configs, evaluation/training tables
- **Abstractions**: Token Budget Manager, Memory–Prompt–Context layers, causal chains
- **Capabilities**: Replay, sandbox, evolution, training pipeline

**Next step**: Implement the **engineering validation roadmap** (§11) to move from design to **provably operational** system. The combination of this design document and the [engineering validation specification](replay-sandbox-evaluation-automation.md) provides a complete path from vision to production-ready implementation.

**Key insight**: The design is **intentionally minimal** to support maximum evolution; the engineering work is to build **automation and quality gates** that make the design operationally complete without requiring schema changes.

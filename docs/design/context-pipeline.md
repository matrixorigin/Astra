# Context Pipeline: A Database-Inspired Architecture for LLM Context Engineering

> **Status**: Design Proposal
> **Author**: XuPeng + Claude
> **Date**: 2026-05-05
> **Builds On**: [prompt-lifecycle.md](prompt-lifecycle.md), [context-window-management.md](context-window-management.md), [context-compact.md](context-compact.md)
> **Related**: [token-efficient-llm-routing.md](token-efficient-llm-routing.md), [session-memory-protocol.md](session-memory-protocol.md)

---

## Thesis

Context assembly for LLM agents is structurally isomorphic to database query execution. Both solve the same fundamental problem: **given finite processing capacity, select and arrange information to maximize output quality**.

This document establishes the theoretical foundation for this claim, derives design principles from a cross-system survey (see Appendix A), and proposes a unified `ContextPipeline` abstraction for astra that makes the analogy explicit and engineerable.

**Implementation stance**: if compatibility with the current request path is required, the pipeline should land first as an **observability and control-plane wrapper** over the existing context assembly path. If compatibility is not required, the better end-state is a **pipeline-first runtime**: `ContextPipeline` becomes the only path that plans, binds, optimizes, serializes, and records feedback for an LLM turn. Even in that aggressive path, the hard constraints are semantic prompt order, provider wire invariants, tool-call/tool-result pairing, and resume/replay correctness — not byte-for-byte compatibility with today's payload builder.

---

## Part I: Theoretical Foundation

### 1.1 The Isomorphism

A database query executor transforms a declarative query into an efficient physical execution plan against bounded resources (memory, I/O, CPU). An LLM context assembler transforms a user intent into an efficient token arrangement against a bounded context window.

```
┌──────────────────────────┬──────────────────────────────────┐
│ Database Query Pipeline  │ LLM Context Pipeline             │
├──────────────────────────┼──────────────────────────────────┤
│ SQL query                │ User message + session state     │
│ Schema catalog           │ Tool schemas + skill registry    │
│ Statistics (cardinality, │ Token estimates, cache hit rates, │
│  selectivity, histogram) │  relevance scores, freshness     │
│ Plan (logical)           │ ContextPlan (what to include)    │
│ Bind (resolve tables,    │ ContextBind (fetch memory, load  │
│  indexes, partitions)    │  history, resolve tools)         │
│ Optimize (reorder joins, │ ContextOptimize (order for cache,│
│  push predicates, choose │  compress history, budget-fit,   │
│  index vs scan)          │  choose compact tier)            │
│ Execute (fetch pages,    │ ContextExecute (serialize blocks,│
│  apply operators, return │  add cache_control, send API     │
│  result set)             │  request)                        │
│ Buffer pool / page cache │ Prompt prefix cache (KV cache)   │
│ EXPLAIN ANALYZE          │ ContextAssemblyTrace             │
│ Adaptive query execution │ LiquidTactical mid-turn adapt    │
└──────────────────────────┴──────────────────────────────────┘
```

The isomorphism is not merely metaphorical. Both systems exhibit:

**1. Resource-bounded optimization**: The context window is a budget analogous to memory grants. Exceeding it is a hard failure (prompt-too-long), just as exceeding memory grants forces spill-to-disk.

**2. Locality and caching**: Prompt prefix caching (Anthropic's KV cache, OpenAI's automatic prefix cache) behaves like a database buffer pool — frequently accessed prefixes remain hot, and the system should be arranged to maximize hit rates. The key insight: **cache hit depends on byte-identical prefixes**, analogous to how buffer pool hits depend on page alignment.

**3. Statistics-driven decisions**: A database optimizer uses table cardinality and index selectivity. A context optimizer should use token estimates, relevance scores, and historical cache hit rates. Without statistics, both degrade to worst-case plans.

**4. Multi-level storage hierarchy**: Just as databases tier data across buffer pool → SSD → HDD → remote storage, context tiers across prompt window → disk persistence → memory service → re-execution (re-running a tool).

### 1.2 Where the Analogy Breaks

The isomorphism has precise boundaries. Acknowledging them prevents over-engineering.

| Property | Database | Context Pipeline | Implication |
|----------|----------|-----------------|-------------|
| **Value quantification** | Cost model: I/O ops, CPU cycles, row estimates | Token counts are measurable; **information value is not** | Cannot build a true cost-based optimizer; must use heuristics + feedback |
| **Determinism** | Same plan, same data → same result | Same context, same model → **different output** (temperature > 0) | Cannot validate optimizations by output equality; must use statistical measures |
| **Schema rigidity** | Fixed schema, typed columns | Unstructured text, no schema enforcement | "Projection pruning" (dropping columns) maps to section-level inclusion/exclusion, not field-level |
| **Query independence** | Queries are stateless | Turns are **deeply stateful** (conversation history is both input and output) | Plan must account for history; compaction is the analogue of garbage collection |
| **Consumer intelligence** | Dumb executor, smart optimizer | **Smart consumer** (LLM reasons about its input) | The consumer can compensate for suboptimal context; recency bias and attention patterns matter |

**Key takeaway**: We cannot build a full cost-based optimizer because we cannot precisely quantify "information value per token." Instead, we build a **heuristic optimizer with observability** — make decisions based on measurable proxies (token count, cache scope, relevance score, recency), and expose enough traces to iterate empirically.

### 1.3 The Cache Alignment Problem

The most practically important aspect of the isomorphism is **cache alignment**. In databases, placing hot rows on the same page maximizes buffer pool efficiency. In LLM context, placing stable content at the start of the prompt maximizes KV cache hits.

Two prompt caching protocols exist in practice:

**Prefix caching** (OpenAI, DeepSeek, GLM): The provider automatically caches the longest matching prefix. Any byte change at position N invalidates everything after N. Implication: **content must be ordered by volatility (most stable first)**.

**Explicit cache control** (Anthropic): The client marks cache breakpoints with `cache_control` blocks. The provider caches up to each breakpoint independently. Implication: **the client controls cache granularity**, but breakpoint placement is a critical decision.

Both protocols share the fundamental constraint: **appending to a cached prefix is free; modifying it is catastrophically expensive** (full recompute from the change point). This is why the "dynamic injection without breaking cache" problem is so central — it's the context engineering equivalent of the buffer pool invalidation problem.

**Astra's current approach**: `CacheScope` enum (Global/Session/None) on each `PromptSection`, plus `ForkPrefix` for child agents. This is a good foundation but lacks explicit cache-alignment optimization (no reordering within scope tiers).

### 1.4 The Pressure Model

Database systems use memory pressure to drive eviction (LRU, clock sweep, etc.). Context pipelines should use **token pressure** to drive compaction. This is already implemented in astra's `AdaptiveCompactConfig::from_pressure()`, but the analogy goes deeper:

```
Pressure = used_tokens / effective_input_limit

Pressure ranges (aligned with current astra AdaptiveCompactConfig):
  0.00–0.60  →  Normal         (buffer pool has free pages; no eviction)
  0.60–0.75  →  TrimSchemas    (start evicting cold pages)
  0.75–0.90  →  CompactHistory (aggressive eviction; consider checkpointing)
  0.90–1.00  →  AggressivePrune (emergency; spill to disk + summarize)
  > 1.00     →  prompt_too_long (OOM equivalent; hard failure → reactive recovery)
```

The database analogy suggests an improvement: **pressure should be predictive, not reactive**. A database checkpoints *before* memory fills; a context pipeline should compact *before* hitting the limit. This means estimating the token cost of the *next turn's response* and compacting proactively.

```
effective_pressure =
  (used_tokens + output_reserve + thinking_reserve + schema_reserve)
  / effective_input_limit
```

The reserve should not be a simple mean. For safety, use a per-model/query-source percentile estimate (p75/p90 for normal turns, p95 after recovery events) with a floor. Averages are too optimistic for long-tail coding turns and can increase `max_output_tokens` or `prompt_too_long` recovery loops.

---

## Part II: Patterns from Practice

A cross-system survey (details in Appendix A) reveals that production agent systems independently converge on the same patterns. None make the database analogy explicit, but the structural similarities are unmistakable.

### 2.1 Convergent Patterns

**Pattern 1: Stable-prefix ordering**

Every system places stable content before volatile content in the system prompt to maximize prefix cache hits:

| Position | Content type | Cache scope | Change frequency |
|----------|-------------|-------------|-----------------|
| 1 (most stable) | Core rules, persona | Global | Weeks/months |
| 2 | Tool schemas, skill catalog | Session | Days |
| 3 | Project context, workspace rules | Session/None | Hours/per-turn |
| 4 | Memory retrieval | None | Per-session |
| Last (most volatile) | Runtime identity, git status | None | Per-turn |

No system explicitly optimizes *within* scope tiers — they rely on hand-tuned ordering. This is the gap that the Optimize phase should fill, but with one hard constraint: **instruction semantics outrank cache locality**. The optimizer may only reorder inside explicitly marked reorderable groups; it must not move core rules, safety constraints, output format, or provider-required blocks across semantic precedence boundaries.

In current astra, some "project context" is built as dynamic profile text (`CacheScope::None`) rather than session-stable context. Promoting any of it to Session scope requires proving that it does not change per turn (for example, git status and runtime identity must stay dynamic).

**Pattern 2: Multi-tier compaction**

Production systems implement 2–4 compaction layers with escalating aggressiveness. The pattern is universal:

| Tier | Strategy | Cost | Recovery |
|------|----------|------|----------|
| 1. Cheapest | Clear old tool results (microcompact) | Free | Placeholder or re-run tool |
| 2. Moderate | Prune tool schemas or trim history tail | Free | Re-fetch if needed |
| 3. Expensive | LLM-based summarization of conversation history | ~2K–20K tokens for summary call | Lossy — original turns gone |
| 4. Emergency | Reactive compact on `prompt_too_long` error | Same as tier 3, but under duress | May lose valuable context |

Astra's advantage: pressure-adaptive parameters that scale continuously (keep_recent: 6→4→2→1, token_budget: 12K→8K→4K→2K) rather than fixed thresholds. This is the right approach — it behaves like a database buffer pool that gradually increases eviction aggressiveness as memory pressure rises.

**Pattern 3: Provider-aware compaction**

Cache protocol differences (prefix vs explicit `cache_control`) change the optimal compaction strategy. Astra parameterizes this via `CompactStrategy`:
- `cache_control` providers → minimal placeholders (`[Cleared]`)
- Prefix cache providers → normalized key=value placeholders that preserve prefix stability

This dual-strategy approach should be preserved and made first-class in the pipeline.

**Pattern 4: Child agent cache sharing**

Multi-agent systems need parent→child cache prefix reuse. Astra's `ForkPrefix` — frozen byte-identical prefix snapshots with per-tool SHA-256 hashing and `validate_spawn` — is the most complete solution observed. The natural extension (§4.3) is peer-to-peer sharing.

**Pattern 5: Latching for cache stability**

Systems that care about cache hit rates implement session-stable latches: once a beta header, cache scope, or feature flag is evaluated, it locks for the session lifetime. The purpose is purely to prevent KV cache invalidation from mid-session mode toggles. This pattern deserves explicit modeling (see §3.3 `SessionLatches`).

**Pattern 6: Emergent context from execution**

Tool execution produces new context that wasn't predictable at prompt assembly time — discovered skills, prefetched memory, tool use summaries, attachment side-effects. Production systems handle this ad-hoc (variables passed between loop iterations). Formalizing it as `EmergentContext` (§3.3) makes the pipeline loop explicit.

### 2.2 Open Problems

**1. Predictive budgeting**: All observed systems react to pressure rather than predicting it. No system estimates the token cost of the upcoming response to compact proactively. The fix is straightforward (§3.4 Phase 1: use running average of response tokens) but nobody implements it.

**2. Cross-turn cache feedback**: No system tracks actual cache hit rates per section and feeds that back into ordering decisions. All rely on static ordering heuristics. With `PipelineStats` (§3.2), this becomes possible.

**3. Information density measurement**: No system measures information value per token (e.g., "this 5K tool result was referenced 3 times in later reasoning; that 8K one was never referenced"). All treat token count as the sole cost metric. This is the hardest open problem (§4.1).

**4. Unified pipeline abstraction**: Every observed system has the pipeline stages scattered across multiple modules. None has a single type that you can `EXPLAIN`. This is the core contribution of this design.

---

## Part III: Proposed Design

### 3.1 Architecture Overview

```
                          User Message + Session State
                                     │
                                     ▼
                        ┌────────────────────────┐
                        │    ContextPlan          │  "What do we need?"
                        │                        │
                        │  • Identify sections    │
                        │  • Estimate budgets     │
                        │  • Select compact tier  │
                        │  • Choose cache strategy│
                        └────────┬───────────────┘
                                 │
                                 ▼
                        ┌────────────────────────┐
                        │    ContextBind          │  "Fetch the data"
                        │                        │
                        │  • Load history         │
                        │  • Retrieve memory      │
                        │  • Resolve tools        │
                        │  • Load skills          │
                        └────────┬───────────────┘
                                 │
                                 ▼
                        ┌────────────────────────┐
                        │    ContextOptimize      │  "Fit it in budget"
                        │                        │
                        │  • Order by cache scope │
                        │  • Compact history      │
                        │  • Prune tool schemas   │
                        │  • Spill oversized data │
                        │  • Set cache breakpoints│
                        └────────┬───────────────┘
                                 │
                                 ▼
                        ┌────────────────────────┐
                        │    ContextExecute       │  "Serialize & send"
                        │                        │
                        │  • Build system blocks  │
                        │  • Normalize messages   │
                        │  • Add cache_control    │
                        │  • Emit trace           │
                        │  • Send API request     │
                        └────────────────────────┘
                                 │
                                 ▼
                        ┌────────────────────────┐
                        │   ContextFeedback       │  "Learn from result"
                        │                        │
                        │  • Record actual usage  │
                        │  • Update cache stats   │
                        │  • Detect cache breaks  │
                        │  • Feed back to Plan    │
                        └────────────────────────┘
```

### 3.2 Core Types

```rust
/// The pipeline — owns the end-to-end flow for one API turn.
/// Stateless: reads configuration and provider strategy, but does NOT own
/// PipelineStats. Stats live in the loop orchestrator and are passed in
/// via ContextSources.stats so that Plan reads immutably and the
/// orchestrator applies feedback after Execute returns.
///
/// Pure data types can live in astra-turn-core; orchestration that reads
/// AgenticLoopState and runtime prompt builders belongs in astra-runtime.
pub struct ContextPipeline {
    config: PipelineConfig,
    provider_strategy: ProviderCacheStrategy,
}

/// Output of the Plan phase — declarative description of what the turn needs.
pub struct ContextPlan {
    pub sections: Vec<PlannedSection>,
    pub estimated_budget: TokenBudget,
    pub compact_tier: CompactionTier,
    pub cache_strategy: CacheStrategy,
    pub pressure: ContextPressure,
}

pub struct PlannedSection {
    pub kind: SectionKind,
    pub scope: CacheScope,             // Global | Session | None
    pub estimated_tokens: u32,
    pub priority: CompressionPriority, // Never | LastResort | Normal | First
    pub source: SectionSource,         // Static | Memory | History | ToolSchema | Skill
}

pub enum SectionKind {
    Identity,             // §1 — agent persona, core rules
    SelfModel,            // §2 — capabilities, learned strengths
    ProjectContext,       // §3 — workspace rules, conventions
    Memory,               // §4 — semantic + episodic recall
    WorkingMemory,        // §5 — scratchpad, active plan
    History,              // §6 — conversation turns
    Constraints,          // §7 — output format, safety rules
    Skills,               // Dynamic — active skill instructions
    RuntimeIdentity,      // Dynamic — model, date, cwd, git
}

/// Output of the Bind phase — concrete content for each section.
pub struct ContextBound {
    pub sections: Vec<BoundSection>,
    pub messages: Vec<Value>,          // Conversation history (possibly compacted)
    pub tools: Vec<ToolSchema>,        // Resolved tool schemas
    pub fork_prefix: Option<ForkPrefix>,
}

pub struct BoundSection {
    pub plan: PlannedSection,
    pub content: String,
    pub actual_tokens: u32,            // Measured after binding
    pub bind_latency: Duration,
}

/// Output of the Optimize phase — ready to serialize.
pub struct ContextOptimized {
    pub system_blocks: Vec<SystemBlock>,  // Ordered, cache-aligned
    pub messages: Vec<Value>,              // Compacted, normalized
    pub tools: Vec<ToolSchema>,            // Possibly pruned
    pub cache_breakpoints: Vec<CacheMarker>, // Provider-specific cache markers
    pub spilled: Vec<SpilledEntry>,        // Content moved to disk
    pub compact_stats: CompactStats,
    pub trace: ContextAssemblyTrace,       // EXPLAIN ANALYZE output
}

/// Feedback from the API response — feeds back into next turn's Plan.
pub struct ContextFeedback {
    pub actual_input_tokens: u32,
    pub cache_creation_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_hit_ratio: f64,          // cache_read / (cache_read + cache_creation)
    pub output_tokens: u32,
    pub was_truncated: bool,
    pub cache_break_detected: Option<CacheBreakReason>,
}

/// Accumulated statistics across turns — the "catalog statistics" of the pipeline.
/// Implementations should bucket these by provider/model/query-source/prefix hash
/// instead of using a single global average for all calls.
pub struct PipelineStats {
    pub turns_executed: u32,
    pub avg_cache_hit_ratio: f64,
    pub section_token_history: HashMap<SectionKind, TokenHistogram>,
    pub compact_events: Vec<CompactEvent>,
    pub cache_breaks: Vec<CacheBreakEvent>,
    pub response_token_estimates: ResponseTokenEstimator,
}

/// Per-model/query-source percentile estimator for predictive reserves.
/// Replaces a naive running average — averages are too optimistic for
/// long-tail coding turns and lead to avoidable PTL recovery loops.
pub struct ResponseTokenEstimator {
    /// Bucketed by (model_id, query_source).
    buckets: HashMap<(String, String), PercentileDigest>,
}

impl ResponseTokenEstimator {
    /// Record an observed response for a given model and query source.
    pub fn record(&mut self, model: &str, source: &str, feedback: &ContextFeedback) { .. }

    /// Compute reserves for the next turn. Uses p75 normally, p95 after recovery.
    pub fn reserve_for(
        &self,
        model: &str,
        source: &str,
        recovery: &RecoveryState,
    ) -> ContextReserves { .. }
}

/// Token reserves subtracted from the context window before pressure is computed.
/// Each field is an independent budget — they sum to the total reserve.
pub struct ContextReserves {
    pub output_tokens: u32,    // Expected model response (p75/p95 of history)
    pub thinking_tokens: u32,  // Extended thinking budget (if enabled)
    pub schema_tokens: u32,    // Tool schema growth headroom
}
```

**Typed artifacts, not just strings**: `BoundSection { content: String }` is a simplification for exposition. The implementation should use typed artifacts for system sections, message patches, tool schemas, attachments, memory snippets, and spill references. This prevents provider metadata, tool-call/tool-result pairing, thinking blocks, and media placeholders from being lost during optimization.

**Implementation ownership**

| Layer | Owns | Notes |
|-------|------|-------|
| `astra-turn-core` | Pure structs, trace schema, pressure/stats helpers, provider cache policy types | Must not depend on `astra-runtime` prompt builders. |
| `astra-runtime` | `AgenticLoopState` adapter, `ContextSources` construction, bind/optimize orchestration | This is where existing prompt, memory, skill, and loop state live today. |
| `astra-prompts` / shared prompt crate | Reusable prompt section builders if they are extracted later | Optional follow-up; do not block Phase A. |
| CLI/server hosts | Display, flags, journal persistence, EXPLAIN UI | Should consume trace output, not own pipeline decisions. |

### 3.3 The Data Source Catalog: Structured Session State

#### The Problem

The Bind phase needs to "query" data sources — but today those sources are scattered across ~170 fields in `AgenticLoopState`, a flat struct that mixes per-turn ephemera, per-session accumulators, per-agent init config, and cross-session learning modules. The pipeline's Plan phase cannot reason about what to fetch because there is no catalog of available sources.

This is the exact analogue of a database's **information_schema** — without it, the query planner cannot enumerate tables, columns, or statistics. The `AgenticLoopState` today is like a database where every table is a global variable.

#### Data Source Taxonomy

Every piece of data that can flow into an LLM context has three properties:

1. **Lifecycle** — when does it change?
2. **Location** — where is it stored?
3. **Bind cost** — how expensive is it to fetch?

A fourth property, **directionality**, turns out to matter too: most data flows strictly *into* the prompt (read-only), but some data is *discovered during execution* and fed back into the next turn. Cross-system analysis reveals **7 distinct lifecycles**:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Context Data Source Catalog (v2)                     │
│                                                                        │
│  Lifecycle        Source                  Location       Bind Cost     │
│  ──────────────────────────────────────────────────────────────────    │
│                                                                        │
│  IMMUTABLE (never changes within agent lifetime)                       │
│  ├── Agent persona            AgentConfig         init-time    free   │
│  ├── Core rules               Static text         compiled     free   │
│  ├── Planning protocol        Static text         compiled     free   │
│  └── Output constraints       Static text         compiled     free   │
│                                                                        │
│  PER-AGENT (changes on agent update, stable within session)            │
│  ├── Tool schemas             ToolRegistry        init-time    free   │
│  ├── Skill catalog            SkillResolver       init-time    free   │
│  ├── Permission context       PermissionHandler   init-time    free   │
│  └── Delegation targets       DelegationEngine    init-time    free   │
│                                                                        │
│  LATCHED (triggered once, then frozen for session — NEW)               │
│  │  DB analogy: one-shot DDL that can't be rolled back within the txn  │
│  ├── Beta headers             bootstrap latch     first-trigger free  │
│  │   └── Auto mode, fast mode, cache editing, thinking clear           │
│  ├── Cache scope eligibility  1h cache allowlist  first-check  free   │
│  │   └── Once determined, never re-evaluated (prevents cache break)    │
│  └── Overcommit eligibility   Usage tier check    first-check  free   │
│      └── Model fallback, overage gate                                  │
│                                                                        │
│  PER-SESSION (changes across sessions, stable within one)              │
│  ├── Edge profile             EdgeProfile         first-turn   free   │
│  │   ├── cwd, git_branch, OS, shell, version                          │
│  │   ├── agent_id, edge_executor_id                                   │
│  │   └── workspace_context (detected language, framework)              │
│  ├── Project context          Local files         first-turn   I/O    │
│  │   └── .astra/rules.md, steering/*.md                                │
│  │       Note: current astra also has per-turn profile text in None     │
│  │       scope; only truly stable files should move here.               │
│  ├── Model config             ModelSelector       first-turn   free   │
│  │   └── model_id, context_window, thinking_config                     │
│  ├── Self-model               Procedural memory   first-turn   I/O    │
│  │   └── learned strengths, tool proficiency                           │
│  └── Compact strategy         ProviderCache       first-turn   free   │
│      └── protocol, placeholder style, breakpoint policy                │
│                                                                        │
│  PER-TURN (changes every turn — the "hot" data)                        │
│  ├── Message thread           AgenticLoopState    in-memory    free   │
│  │   ├── messages: Vec<Value>  (full history)                          │
│  │   ├── tool_results: Vec<Value>  (pending callbacks)                 │
│  │   └── final_text: Option<String>                                    │
│  ├── Token accounting         AgenticLoopState    in-memory    free   │
│  │   ├── prompt_tokens, cache_read, cache_creation, completion         │
│  │   └── cumulative totals across turns                                │
│  ├── Session facts (L1a)      SessionFacts        in-memory    free   │
│  │   ├── active_files (max 20, with action + turn)                     │
│  │   ├── recent_tool_calls (max 10)                                    │
│  │   ├── plan_state (goal, progress, current subtask)                  │
│  │   ├── blocked_tools, error_state                                    │
│  │   └── turn, estimated_tokens                                        │
│  ├── Continuity state         ContinuityState     in-memory    free   │
│  │   └── goal, todos, facts, user_corrections, verification           │
│  ├── Active skills            SkillState          in-memory    free   │
│  │   └── detected from message, resolved from registry                 │
│  ├── Recent file reads        FileStateCache      in-memory    free   │
│  │   └── path→{content, timestamp, offset}, LRU eviction               │
│  │       Currently path→turn only; upgrade to content-aware (see §3.3) │
│  ├── Dedup caches             IdempotencyCache    in-memory    free   │
│  │   └── semantic_dedup, call_counts, per-tool caps                    │
│  └── Recovery state           ErrorRecovery       in-memory    free   │
│      └── consecutive_errors, PTL retry count, max_output escalation    │
│      Influences next Plan: after PTL error → Plan must compact harder  │
│                                                                        │
│  EXTERNAL (not in session — fetched on demand)                         │
│  ├── Semantic memory          Memoria (HTTP)      remote       ~50ms  │
│  │   └── memory_retrieve(query, top_k, session_id)                     │
│  ├── Episodic memory          Memoria (HTTP)      remote       ~50ms  │
│  │   └── Prior session summaries, goal progress                        │
│  ├── Spilled tool results     Disk                local I/O    ~1ms   │
│  │   └── ~/.astra/sessions/<id>/tool-results/<call-id>.txt             │
│  ├── Cloud context snapshots  MatrixOne (HTTP)    remote       ~100ms │
│  │   └── Cross-session context, audit replay                           │
│  └── ForkPrefix (parent)      Parent agent        in-process   free   │
│      └── Frozen system blocks, tool schemas, prefix hash               │
│                                                                        │
│  EMERGENT (discovered during execution — NOT plannable — NEW)          │
│  │  DB analogy: deferred/adaptive execution — query discovers new      │
│  │  data sources mid-flight and feeds them into the next iteration     │
│  ├── Skill discovery          Prefetch during streaming      ~20ms    │
│  │   └── Writing to file X triggers discovery of skill Y               │
│  │       Prefetched during model streaming, injected next turn         │
│  ├── Memory prefetch          Prefetch during streaming      ~50ms    │
│  │   └── While model streams, fetch relevant memory for next turn      │
│  │       Consumed once on the next loop iteration                      │
│  ├── Attachment messages      Tool execution side-effects    free     │
│  │   └── Tool results that generate new context (file diffs,           │
│  │       MCP resources, deferred tools, queued commands)               │
│  └── Tool use summaries       Async LLM during turn          ~200ms  │
│      └── Small-model summary of tool calls, generated this turn,       │
│          injected next turn as compressed tool call narrative           │
│                                                                        │
│  DERIVED (computed from other sources — never stored)                   │
│  ├── Context pressure         f(token_accounting, model_config)  free  │
│  ├── Compaction tier          f(pressure)                        free  │
│  ├── Runtime identity text    f(edge_profile, model, date)       free  │
│  └── Self-model text          f(tool_schemas, skills, memory)    free  │
│                                                                        │
└─────────────────────────────────────────────────────────────────────────┘
```

Two lifecycle tiers deserve special attention because they break the simple "stable → volatile" ordering assumption:

**LATCHED** — State that is evaluated lazily on first trigger, then frozen for the session. The key property: latches exist specifically to **prevent cache breaks**. If a beta header or cache scope could toggle mid-session, it would invalidate the KV cache prefix. The latch makes the state "eventually immutable" — it starts as `None`, becomes `Some(value)` once, and never changes again. This is distinct from PER-SESSION (which is set at session start) because latches fire at unpredictable times during the session. The DB analogy is a one-shot DDL (`CREATE INDEX CONCURRENTLY`) — once committed, it changes the execution plan permanently, and rolling it back would be catastrophically expensive.

**EMERGENT** — Context that is *discovered during execution*, not predictable at Plan time. The pipeline model (Plan → Bind → Optimize → Execute) assumes all data sources are known before execution begins. But in practice, execution produces new context:

- Tool execution reveals files that trigger skill discovery
- Model streaming runs concurrently with memory prefetching
- Async small-model calls generate tool use summaries for the *next* turn
- Attachment messages (file diffs, MCP resources) emerge as side-effects

This is the context-engineering analogue of **adaptive query execution** in modern databases (e.g., Spark AQE, SQL Server Intelligent QP). The query plan is partially re-optimized mid-flight as the executor discovers that row estimates were wrong.

For the context pipeline, the implication is: **the pipeline is not a single-pass process**. A turn may run the pipeline once for the LLM call, but then produce emergent context (attachments, prefetched memory, skill discoveries) that is queued for the *next* pipeline invocation. The feedback loop is not just Plan→Execute→Feedback; it's Plan→Execute→Discover→Plan(next turn).

#### The SessionState Abstraction

The insight is: **not all of this belongs in one struct**. Just as a database separates `pg_catalog` (system tables), `pg_stat` (runtime statistics), and user tables (data), the context pipeline should separate its data sources by lifecycle and ownership.

```rust
/// The "catalog" that the pipeline queries during Plan and Bind.
/// Replaces the flat AgenticLoopState as the pipeline's interface.
///
/// 8 tiers, ordered by volatility (most stable first).
/// The pipeline Plan phase reads only tiers it needs;
/// the Bind phase fetches from all tiers that Plan selected.
pub struct ContextSources<'a> {
    // ── IMMUTABLE (compiled into binary, zero-cost access) ──
    pub statics: &'a StaticSections,

    // ── PER-AGENT (set at agent init, stable for agent lifetime) ──
    pub agent: &'a AgentContext,

    // ── LATCHED (triggered once mid-session, then frozen) ──
    pub latches: &'a SessionLatches,

    // ── PER-SESSION (set at session start, stable within session) ──
    pub session: &'a SessionContext,

    // ── PER-TURN (mutated every turn — the "working set") ──
    pub turn: &'a TurnState,

    // ── EXTERNAL (fetched on demand — the "remote tables") ──
    pub external: &'a ExternalSources,

    // ── EMERGENT (produced by previous turn's execution — queued for this turn) ──
    pub emergent: &'a EmergentContext,

    // ── FEEDBACK (accumulated across turns — the "statistics") ──
    pub stats: &'a PipelineStats,
}

/// Pre-compiled static text sections. Immutable after build.
/// Analogy: system catalog tables (pg_proc, pg_type).
pub struct StaticSections {
    pub core_rules: PromptSection,        // §1 Identity — never changes
    pub planning_protocol: PromptSection,
    pub coding_discipline: PromptSection,
    pub turn_discipline: PromptSection,
    pub parallel_efficiency: PromptSection,
    pub output_format: PromptSection,     // §7 Constraints
    pub tool_error_recovery: PromptSection,
}

/// Agent-level context. Set at init, survives across sessions.
/// Analogy: schema definition (CREATE TABLE).
pub struct AgentContext {
    pub agent_id: String,
    pub persona: String,                  // Agent-specific system prompt
    pub tool_registry: ToolRegistry,      // Available tools + schemas
    pub skill_catalog: SkillCatalog,      // Available skills
    pub permission_ctx: PermissionContext,
    pub delegation_targets: Vec<AgentRef>,
}

/// Session-stable latches. Each field starts as None, becomes Some(value)
/// on first trigger, and NEVER changes again for the session lifetime.
///
/// Analogy: one-shot DDL (CREATE INDEX CONCURRENTLY) — once committed,
/// it changes the execution plan permanently. Rolling back would
/// invalidate the KV cache prefix, which is catastrophically expensive.
///
/// Why a separate tier instead of folding into SessionContext:
/// - SessionContext is populated at session start (deterministic)
/// - Latches fire at unpredictable times (first auto-mode, first tool error, etc.)
/// - Latches must be checked by the Optimize phase for cache alignment —
///   a flipped latch means the prefix hash changes, so Optimize needs to
///   know whether it was already set on the previous turn.
pub struct SessionLatches {
    /// Beta headers that, once sent, must be sent on every subsequent turn.
    /// E.g., auto-mode header, fast-mode header, cache-editing header.
    pub beta_headers: Vec<LatchedHeader>,

    /// Cache scope eligibility (e.g., 1h TTL, global scope).
    /// Evaluated once, then frozen to prevent mid-session scope flips.
    pub cache_scope: Option<CacheScope>,

    /// Provider-specific feature gates that affect serialization.
    /// E.g., thinking mode clearing after idle timeout.
    pub provider_features: Vec<LatchedFeature>,
}

pub struct LatchedHeader {
    pub name: String,
    pub value: String,
    pub latched_at_turn: u32,  // For diagnostics: when was it triggered?
}

pub struct LatchedFeature {
    pub key: String,
    pub latched_at_turn: u32,
}

/// Session-level context. Set at session start, stable within session.
/// Analogy: connection-level settings (SET search_path, timezone).
pub struct SessionContext {
    pub session_id: String,
    pub run_id: String,
    pub edge_profile: EdgeProfile,        // cwd, git, OS, shell
    pub project_context: ProjectContext,   // .astra/rules.md, steering
    pub model_config: ModelConfig,         // model_id, context_window, thinking
    pub compact_strategy: ProviderCacheStrategy,
    pub self_model: Option<SelfModelSnapshot>, // Learned strengths (refreshed daily)
}

/// Per-turn mutable state. Changes every turn.
/// Analogy: transaction-local state (temp tables, cursors).
pub struct TurnState {
    // Message thread
    pub messages: Vec<Value>,
    pub tool_results: Vec<Value>,

    // Token accounting (the 4 disjoint buckets)
    pub tokens: TokenAccounting,

    // Ground truth (the agent's "working memory")
    pub facts: SessionFacts,
    pub continuity: ContinuityState,

    // Active context
    pub active_skills: Vec<ActiveSkill>,

    // File content cache — not just recency, but actual content for dedup.
    // Currently tracks only path→turn; upgrading to content-aware enables:
    //   1. Dedup: skip re-reading files whose content hasn't changed
    //   2. Spill quality: persist meaningful content, not just filenames
    //   3. Optimize: estimate token savings from deduplication
    pub file_cache: FileStateCache,

    // Control
    pub remaining_turns: u32,
    pub turn_index: u32,

    // Dedup (prevents redundant tool calls within the turn)
    pub dedup: DedupState,

    // Recovery state — influences next Plan phase.
    // After a PTL error, Plan must compact more aggressively.
    // After max_output escalation, Plan adjusts output token reserve.
    pub recovery: RecoveryState,
}

pub struct RecoveryState {
    pub consecutive_ptl_errors: u32,       // prompt_too_long error streak
    pub has_attempted_reactive_compact: bool,
    pub max_output_escalation_count: u32,  // 8K→64K retry count
    pub consecutive_same_errors: u32,      // same error type in a row
}

/// External data sources — fetched on demand, not owned by the pipeline.
/// Analogy: foreign data wrappers (postgres_fdw), dblinks.
pub struct ExternalSources {
    pub memoria: Option<MemoriaClient>,        // Semantic + episodic memory
    pub spill_dir: Option<PathBuf>,            // Disk persistence for tool results
    pub cloud_snapshots: Option<CloudClient>,   // Cross-session context
    pub fork_prefix: Option<ForkPrefix>,        // Parent agent's cache prefix
}

/// Context discovered during the PREVIOUS turn's execution.
/// Not predictable at Plan time — the pipeline must handle it as "late-arriving data."
///
/// Analogy: adaptive query execution (Spark AQE). The executor discovers
/// that partition sizes differ from estimates, and re-optimizes mid-flight.
/// Here, the previous turn's execution discovered new context that should
/// be included in the current turn's prompt.
///
/// Lifecycle: populated by Execute(turn N), consumed by Bind(turn N+1), then cleared.
///
/// ### Safety triple: TTL + dedup + cap
///
/// Emergent context is uniquely dangerous because it flows *backward* from Execute
/// to Bind. Without guardrails, stale attachments accumulate, duplicates inflate
/// token usage, and unbounded lists cause the next turn to overflow.
///
/// Every item in EmergentContext carries:
/// - **TTL** (`created_at_turn`): Bind skips items older than `current_turn - max_age`.
///   Default max_age = 1 (consume on the immediately next turn only).
///   Items that survive past TTL are dropped, not silently injected.
/// - **Dedup key** (`content_hash`): Before insertion, Execute checks for an existing
///   item with the same hash. Duplicates are dropped at write time, not at read time.
///   This prevents the "resume/replay double-inject" problem.
/// - **Per-list cap**: Each Vec is capped. Excess items are dropped oldest-first.
///   Caps are intentionally small — emergent context is supplementary, not primary.
pub struct EmergentContext {
    /// Skills discovered from tool execution (e.g., file write triggered skill detection).
    /// Prefetched during model streaming, injected as attachment next turn.
    /// Cap: 4 skills per turn.
    pub discovered_skills: Vec<EmergentItem<DiscoveredSkill>>,

    /// Memory prefetched during model streaming.
    /// Runs concurrently with LLM response, consumed once on the next loop iteration.
    /// Cap: 8 entries per turn.
    pub prefetched_memory: Vec<EmergentItem<PrefetchedMemory>>,

    /// Tool use summaries generated asynchronously by a smaller model.
    /// Generated this turn, injected next turn as compressed tool call narrative.
    /// Cap: 1 summary per turn (latest wins).
    pub tool_summaries: Vec<EmergentItem<ToolUseSummary>>,

    /// Attachment-style context from tool execution side-effects.
    /// Types: edited_text_file, skill_discovery, queued_command,
    /// memory, mcp_resources, deferred_tools.
    /// Cap: 16 attachments per turn.
    pub attachments: Vec<EmergentItem<Attachment>>,
}

/// Wrapper that enforces TTL and dedup for every emergent item.
pub struct EmergentItem<T> {
    pub value: T,
    pub created_at_turn: u32,  // TTL anchor — Bind skips if current_turn - created_at_turn > max_age
    pub content_hash: u64,     // Dedup key — Execute rejects duplicates at write time
}

impl EmergentContext {
    /// Insert an item, enforcing dedup (by hash) and cap (drop oldest on overflow).
    pub fn push_skill(&mut self, item: EmergentItem<DiscoveredSkill>) { .. }
    pub fn push_memory(&mut self, item: EmergentItem<PrefetchedMemory>) { .. }
    pub fn push_summary(&mut self, item: EmergentItem<ToolUseSummary>) { .. }
    pub fn push_attachment(&mut self, item: EmergentItem<Attachment>) { .. }

    /// Drain items that are within TTL. Clears the consumed items.
    /// Called by Bind at the start of each turn.
    pub fn drain_live(&mut self, current_turn: u32, max_age: u32) -> EmergentContext { .. }
}
```

**Design note on EmergentContext**: The original pipeline assumed `Plan → Bind → Optimize → Execute` is a single pass. EmergentContext breaks this assumption — it's the output of `Execute(N)` that becomes input to `Bind(N+1)`. The pipeline is actually a **loop with carry-forward state**:

```
Turn N:   Plan → Bind → Optimize → Execute → [discover emergent context]
                                                        │
Turn N+1: Plan → Bind(+ emergent from N) → Optimize → Execute → [discover]
                                                                      │
Turn N+2: Plan → Bind(+ emergent from N+1) → ...
```

This is why `EmergentContext` is a separate tier in `ContextSources` rather than being folded into `TurnState` or `ExternalSources`:
- It's **not external** — it's already in-process, produced by the previous turn
- It's **not per-turn state** — it's not mutated during the turn; it's consumed once and cleared
- It's **directionally unique** — it flows backward from Execute to Bind, opposite to normal data flow

#### Why This Decomposition Matters for the Pipeline

Each pipeline phase interacts with different tiers of the catalog:

```
Phase       Reads from                    Writes to
──────────────────────────────────────────────────────────
Plan        turn.tokens                   (nothing — pure function)
            turn.recovery
            session.model_config
            latches.cache_scope
            stats.*

Bind        statics.*                     (produces BoundSection list)
            agent.tool_registry
            agent.skill_catalog
            session.edge_profile
            session.project_context
            session.self_model
            turn.messages
            turn.facts
            turn.active_skills
            turn.file_cache
            external.memoria              (async I/O)
            external.fork_prefix
            emergent.*                    (consumed once, then cleared)

Optimize    (operates on BoundSections)   turn.messages (compacts in-place)
            session.compact_strategy      external.spill_dir (spill to disk)
            latches.*

Execute     (operates on Optimized)       stats.* (records feedback)
            latches.*                     turn.tokens (updates from response)
                                          emergent.* (populates for NEXT turn)

Feedback    (API response)                stats.* (cache hits, breaks)
                                          latches.* (may trigger new latch)
```

The key properties:

**Pressure planning is a pure function** of `turn.tokens + turn.recovery + session.model_config + latches.cache_scope + stats`. It needs no I/O. This means the pressure/tier/reserve decision can run synchronously and deterministically — exactly like the cheap front half of a database planner. The addition of `turn.recovery` is important: after a prompt-too-long error, the planner must select a more aggressive compaction tier than pressure alone would dictate.

The full `ContextPlan.sections` is not purely derivable from token pressure alone. It also depends on already-resolved tool visibility, active skills, task type, output style, prompt overrides, and provider/model constraints. Treat this as a separate `AssemblyIntent` or manifest step so the implementation does not force dynamic runtime state into the pure pressure planner.

**Bind does all the I/O**, and each binding is independent. This means all bindings can run concurrently via `tokio::join!`. The external sources (Memoria, disk, cloud) are the "slow" bindings; everything else is in-memory and essentially free. EmergentContext bindings are also free (already in-process), but they're logically separate because they represent data that the *previous* turn's execution discovered.

**Optimize reads latches** — latched beta headers and cache scope affect how system blocks are constructed and where cache breakpoints are placed. A latch that fired during the previous turn means the prefix hash has changed, and Optimize must account for that (this is a potential cache break, but one that was already accepted at latch time).

**Execute produces emergent context** — this is the key architectural insight that the v1 design missed. Execute doesn't just produce `ContextFeedback` (statistics); it also produces `EmergentContext` (new data sources for the next turn). This makes the pipeline a **loop with carry-forward**, not a single pass.

This separation makes the pipeline **testable**: you can unit-test Plan with synthetic `TurnState` + `RecoveryState`, test Bind with mock `ExternalSources` + canned `EmergentContext`, and test Optimize with pre-built `BoundSection` lists + `SessionLatches`. No integration test needs all tiers simultaneously.

#### Migration Path from AgenticLoopState

The ~170 fields in `AgenticLoopState` map to the new structure as follows:

| Current field cluster | New home | Notes |
|----------------------|----------|-------|
| messages, tool_results, final_text | `TurnState` | Core message thread |
| 4 token buckets + aggregators | `TurnState.tokens` | TokenAccounting struct |
| session_id, run_id, recursion_depth | `SessionContext` | Stable per session |
| restricted_tools, boosted_tools | `AgentContext` (init) or `TurnState` (dynamic) | Split by lifecycle |
| skills registry/resolver/executor | `AgentContext.skill_catalog` | Per-agent, not per-turn |
| delegation_engine, permissions | `AgentContext` | Stable per agent |
| edge_profile, project_context | `SessionContext` | Set once at session start |
| SessionFacts, ContinuityState | `TurnState.facts`, `TurnState.continuity` | Per-turn ground truth |
| compact_strategy, thinking | `SessionContext` | Stable per session |
| consecutive_context_window_errors | `TurnState.recovery` | NEW: influences Plan compaction tier |
| recent_file_reads (path→turn) | `TurnState.file_cache` | UPGRADED: add content + LRU eviction |
| (no current equivalent) | `SessionLatches` | NEW: extracted from ad-hoc header logic |
| (no current equivalent) | `EmergentContext` | NEW: extracted from implicit attachment flow |
| PipelineStats (NEW) | `ContextSources.stats` | NEW: accumulated feedback |
| tactical_adapter, step_signals | Orthogonal to pipeline (lives in turn loop) | Not in ContextSources |
| messaging, hooks, cancellation | Orthogonal to pipeline (execution concerns) | Not in ContextSources |

**What stays out of `ContextSources`**: Execution-layer concerns (hooks, cancellation, messaging, interruption, harness) are not data sources for context assembly. They remain in the turn loop orchestrator. The pipeline only sees data it might include in the prompt.

**What's genuinely new** (not just reorganization):
- `SessionLatches` — currently ad-hoc header/flag logic scattered across turns; formalizing it enables Optimize to reason about cache alignment
- `EmergentContext` — currently implicit (attachments, prefetches stored in ad-hoc variables); formalizing it makes the pipeline loop explicit
- `RecoveryState` — currently side-channel (error counters buried in loop state); surfacing it to Plan enables "recover harder" compaction
- `FileStateCache` upgrade — currently path→turn; upgrading to content-aware enables dedup optimization in Bind

**The 80/20 rule**: ~45 of the ~170 fields are actual data sources for context assembly (up from the v1 estimate of ~40, because latches and emergent context were previously invisible). The remaining ~125 are execution machinery. The `ContextSources` abstraction captures the 45 that matter for the pipeline, without absorbing the 125 that don't.

### 3.4 Phase Details

#### Phase 1: Plan

The pressure-planning part of Plan is **pure computation** — no I/O, no async. It examines `ContextSources` (specifically `turn.tokens` and `session.model_config`) plus `PipelineStats` to produce pressure, reserve, budget, and compaction decisions. Section manifests may still depend on already-resolved runtime inputs such as tool visibility and task type.

```rust
impl ContextPipeline {
    pub fn plan(&self, sources: &ContextSources) -> ContextPlan {
        let stats = sources.stats;

        // 1. Compute pressure (turn tokens + model limit + predictive reserves)
        let reserves = stats.response_token_estimates.reserve_for(
            sources.session.model_config.model_id(),
            sources.session.query_source(),
            &sources.turn.recovery,
        );
        let pressure = ContextPressure::compute(
            sources.turn.tokens.total_input(),
            sources.session.model_config.effective_input_limit(),
            reserves,
        );

        // 2. Select compaction tier from pressure + recovery state.
        //    Gating rule: predictive pressure can only INCREASE the tier, never
        //    decrease it below what raw pressure alone would select.
        //    This prevents over-compaction from a temporarily inflated reserve estimate.
        let raw_tier = select_compaction_tier(pressure.raw);
        let predictive_tier = select_compaction_tier(pressure.value);
        let tier = predictive_tier
            .max(raw_tier)  // predictive can escalate, never de-escalate
            .escalate_for_recovery(&sources.turn.recovery);

        // 3. Plan sections with budget allocation
        let budget = TokenBudget::allocate(
            sources.session.model_config.effective_input_limit(),
            tier,
            &stats.section_token_history,
        );

        // 4. Determine cache strategy from provider policy + latches + accumulated stats
        let cache_strategy = self.provider_strategy.select_cache_strategy(
            stats,
            tier,
            sources.latches.cache_scope.as_ref(),
        );

        let sections = self.plan_section_manifest(sources, &budget, tier, &cache_strategy);

        ContextPlan { sections, estimated_budget: budget, compact_tier: tier,
                       cache_strategy, pressure }
    }
}
```

**Predictive pressure** (new vs current astra):

```rust
impl ContextPressure {
    pub fn compute(
        current_tokens: u32,
        limit: u32,
        reserves: ContextReserves,
    ) -> Self {
        let raw = current_tokens as f64 / limit as f64;
        // Predictive: account for expected response, thinking, and schema growth.
        let predictive = (
            current_tokens as f64
            + reserves.output_tokens as f64
            + reserves.thinking_tokens as f64
            + reserves.schema_tokens as f64
        ) / limit as f64;
        Self {
            value: predictive,  // Use predictive for tier selection
            raw,                // Expose raw for diagnostics
        }
    }
}
```

#### Phase 2: Bind

The Bind phase performs **all I/O** — memory retrieval, history loading, tool resolution. Each binding is independent and can run concurrently. It reads from `ContextSources` but never mutates it.

```rust
impl ContextPipeline {
    pub async fn bind(&self, plan: &ContextPlan, sources: &ContextSources<'_>) -> ContextBound {
        // ── EXTERNAL bindings (async, concurrent) ──
        // These are the "slow" bindings — network I/O to Memoria, disk reads, etc.
        let (memory, spill_recovery) = tokio::join!(
            self.bind_memory(plan, sources.external.memoria.as_ref(), &sources.turn),
            self.bind_spill_recovery(plan, sources.external.spill_dir.as_deref(), &sources.turn),
        );

        // ── IN-MEMORY bindings (sync, essentially free) ──
        // Static sections from compiled text (zero cost)
        let identity = self.bind_identity(sources.statics);
        let constraints = self.bind_constraints(sources.statics);

        // Agent-level (stable for agent lifetime)
        let tools = self.bind_tools(plan, &sources.agent.tool_registry);
        let skills = self.bind_skills(plan, &sources.agent.skill_catalog, &sources.turn.active_skills);

        // Session-level (stable within session)
        let self_model = self.bind_self_model(plan, &sources.session, &tools);
        let project_ctx = self.bind_project_context(&sources.session.project_context);
        let runtime = self.bind_runtime_identity(&sources.session.edge_profile, &sources.session.model_config);

        // Turn-level (current state)
        let (history_section, messages) = self.bind_history(plan, &sources.turn);

        // Emergent context from previous turn (consumed once, then cleared)
        let emergent_skills = self.bind_emergent_skills(&sources.emergent);
        let emergent_memory = self.bind_emergent_memory(&sources.emergent);
        let emergent_summaries = self.bind_emergent_summaries(&sources.emergent);

        ContextBound { sections: vec![identity, self_model, project_ctx, memory,
                                       emergent_memory, skills, emergent_skills,
                                       history_section, constraints, runtime, emergent_summaries],
                        messages,
                        tools, fork_prefix: sources.external.fork_prefix.clone() }
    }
}
```

Memory binding respects the plan's budget allocation, querying `ExternalSources.memoria`:

```rust
async fn bind_memory(
    &self,
    plan: &ContextPlan,
    memoria: Option<&MemoriaClient>,
    turn: &TurnState,
) -> BoundSection {
    let Some(client) = memoria else { return BoundSection::empty(SectionKind::Memory); };
    let budget = plan.memory_budget();
    let results = client.retrieve(
        &turn.last_user_message(),
        top_k: budget.max_entries,
    ).await;

    // Trim to fit token budget
    let mut content = String::new();
    let mut tokens = 0;
    for mem in results {
        let est = estimate_tokens(&mem.content);
        if tokens + est > budget.max_tokens { break; }
        content.push_str(&mem.content);
        tokens += est;
    }
    BoundSection { content, actual_tokens: tokens, ... }
}
```

#### Phase 3: Optimize

The Optimize phase transforms bound content into a cache-aligned, budget-fitted, provider-specific arrangement. **No I/O** (except spill-to-disk, which is local).

**Optimizer limits**: Optimize is not a general-purpose rewriter. It operates within a strict budget of allowed transformations, each gated by a boolean in `OptimizeLimits`:

```rust
pub struct OptimizeLimits {
    /// Reorder sections within explicitly marked reorderable groups.
    /// Never reorder across semantic precedence boundaries.
    pub allow_reorder: bool,           // default: false until validated per provider

    /// Clear old tool results (microcompact).
    pub allow_tool_result_clearing: bool,  // default: true

    /// Prune tool schemas under pressure.
    pub allow_schema_pruning: bool,    // default: true above TrimSchemas tier

    /// Spill oversized content to disk.
    pub allow_spill: bool,             // default: true

    /// LLM-based history summarization (expensive, lossy).
    pub allow_llm_summary: bool,       // default: true above CompactHistory tier

    /// Drop entire API rounds (emergency only).
    pub allow_round_dropping: bool,    // default: true only at AggressivePrune

    /// Maximum number of sections that can be reordered per turn.
    /// Prevents a single optimize call from reshuffling the entire prompt.
    pub max_reorder_moves: u32,        // default: 2

    /// Maximum tokens that can be cleared in a single optimize call.
    /// Circuit breaker: if clearing would exceed this, stop and emit a trace warning.
    pub max_clear_tokens: u32,         // default: effective_input_limit / 2
}
```

Each optimization step checks its gate before acting. If a gate is closed, the step is skipped and the trace records `skipped(reason)`. This makes the optimizer auditable: `EXPLAIN` shows not just what it did, but what it *could have done* and why it didn't.

Cache placement must be policy-driven. Anthropic-style cache control, Bedrock cache points, OpenAI-compatible prefix caching, and forked child-agent cache reuse have different marker limits and different failure modes. The optimizer should consume a `ProviderCachePolicy` rather than hard-code "Global breakpoint + Session breakpoint":

| Policy field | Purpose |
|--------------|---------|
| `protocol` | Prefix-only, Anthropic cache_control, Bedrock cachePoint, OpenAI-compatible metadata. |
| `max_markers` | Some providers/backends perform best with one marker; others support multiple stable blocks. |
| `marker_granularity` | System block, message, content block, or request-envelope marker. |
| `supports_global_scope` | Whether `scope: global` or equivalent is legal and beneficial. |
| `supports_cache_reference` | Whether cached tool results can be referenced instead of resent. |
| `skip_cache_write_behavior` | Fork/side-query behavior that should reuse a prefix without polluting the main cache tail. |

```rust
impl ContextPipeline {
    pub fn optimize(
        &self,
        plan: &ContextPlan,
        bound: ContextBound,
        latches: &SessionLatches,
    ) -> ContextOptimized {
        let ContextBound {
            mut sections,
            mut messages,
            mut tools,
            fork_prefix,
        } = bound;

        // 1. ORDER: Preserve semantic order, then align cache-safe groups.
        //    Only sections explicitly marked reorderable may move within their
        //    semantic group. Core rules and provider-required blocks are fixed.
        sections = self.cache_align_semantic_groups(sections, &plan.cache_strategy);

        // 2. COMPACT: Apply tier-appropriate compaction to history
        let compact_stats = match plan.compact_tier {
            CompactionTier::Normal => CompactStats::noop(),
            CompactionTier::TrimSchemas => {
                self.prune_tool_schemas(&mut tools, plan.pressure);
                CompactStats::schema_prune(...)
            }
            CompactionTier::CompactHistory => {
                compact_tool_results_adaptive(
                    &mut messages,
                    plan.pressure.value,
                    self.provider_strategy.compact_strategy,
                )
            }
            CompactionTier::AggressivePrune => {
                // LLM summary + drop oldest rounds
                self.aggressive_compact(&mut messages, &mut sections)
            }
        };

        // 3. SPILL: Persist oversized content to disk before clearing
        let spilled = self.spill_oversized(&mut messages, &plan);

        let fork_prefix = fork_prefix.as_ref();

        // 4. CACHE ALIGN: Place cache markers based on provider policy
        //    Latches affect this: latched beta headers and cache scope determine
        //    which blocks get cache_control markers and what scope they use.
        let (system_blocks, breakpoints) = match self.provider_strategy.prompt_cache_protocol {
            PromptCacheProtocol::AnthropicCacheControl => {
                self.build_anthropic_blocks(&sections, &plan.cache_strategy, latches, fork_prefix)
            }
            PromptCacheProtocol::Prefix => {
                // Prefix caching: latched headers don't affect block construction,
                // but they DO affect the request envelope (sent as HTTP headers).
                self.build_prefix_blocks(&sections, fork_prefix)
            }
        };

        // 5. TRACE: Build the EXPLAIN ANALYZE output
        let trace = self.build_trace(&plan, &sections, &messages, &tools, &compact_stats);

        ContextOptimized { system_blocks, messages, tools,
                           cache_breakpoints: breakpoints, spilled, compact_stats, trace }
    }
}
```

#### Phase 4: Execute

The Execute phase is mechanical — serialize and send. Separated from Optimize so that the optimized plan can be inspected (`EXPLAIN` without `ANALYZE`).

```rust
impl ContextPipeline {
    pub async fn execute(&self, optimized: ContextOptimized) -> (ApiResponse, ContextFeedback) {
        let request = self.serialize_request(&optimized);

        // Optional: EXPLAIN mode — return trace without executing
        if self.config.explain_only {
            return (ApiResponse::ExplainOnly(optimized.trace), ContextFeedback::none());
        }

        let response = self.api_client.send(request).await;

        // Build feedback from response usage.
        // The caller is responsible for feeding this into PipelineStats —
        // the pipeline itself holds only an immutable &PipelineStats via ContextSources.
        let feedback = ContextFeedback::from_response(&response, &optimized.trace);

        (response, feedback)
    }
}
```

#### Phase 5: Feedback (the closed loop)

The Feedback phase updates `PipelineStats`, enabling the Plan phase to make better decisions on the next turn. This is the analogue of a database's statistics collector.

```rust
impl PipelineStats {
    pub fn record(&mut self, model: &str, source: &str, feedback: &ContextFeedback) {
        self.turns_executed += 1;

        // Track cache hit ratio (exponential moving average)
        self.avg_cache_hit_ratio = 0.9 * self.avg_cache_hit_ratio
                                 + 0.1 * feedback.cache_hit_ratio;

        // Feed response token estimator (bucketed by model + source)
        self.response_token_estimates.record(model, source, feedback);

        // Record cache breaks for diagnostics
        if let Some(reason) = &feedback.cache_break_detected {
            self.cache_breaks.push(CacheBreakEvent {
                turn: self.turns_executed,
                reason: reason.clone(),
                impact_tokens: feedback.cache_creation_tokens,
            });
        }
    }
}
```

### 3.5 EXPLAIN ANALYZE

The pipeline's observability mode mirrors SQL's `EXPLAIN ANALYZE`:

```
┌─ EXPLAIN ─────────────────────────────────────────────────────────────────┐
│                                                                           │
│  ContextPlan                                                              │
│    pressure: 0.72 (raw: 0.65)                                            │
│    reserves: output=480 thinking=0 schema=120  (p75, normal)             │
│    tier: CompactHistory                                                   │
│    cache_strategy: AnthropicCacheControl (2 markers)                     │
│                                                                           │
│  Sections (planned → actual tokens):                                      │
│    §1 Identity          [Global]    →  480 tok  (never compress)          │
│    §2 SelfModel         [Session]   →  320 tok  (last resort)             │
│    §3 ProjectContext     [Session]   →  150 tok  (last resort)            │
│    §4 Memory            [None]      →  890 tok / 1200 budget (4 entries)  │
│    §5 WorkingMemory     [None]      →    0 tok  (no active plan)          │
│    §6 History           [None]      → 3200 tok / 5000 budget (12 turns)   │
│    §7 Constraints       [Global]    →  280 tok  (never compress)          │
│    Skills: code_review  [Session]   →  650 tok                            │
│    RuntimeIdentity      [None]      →  120 tok                            │
│                                                                           │
│  Compaction:                                                              │
│    tier=CompactHistory, keep_recent=2, token_budget=4000                  │
│    cleared 5 tool results (3200 → 850 tokens), 3 spilled to disk         │
│                                                                           │
│  Tools: 12 available → 12 selected (no pruning at this tier)             │
│                                                                           │
│  Cache markers:                                                           │
│    marker[0]: after §1+§7 (Global scope, 760 tok)                        │
│    marker[1]: after §2+§3+Skills (Session scope, 1880 tok)               │
│                                                                           │
│  Total: 6090 / 8500 effective budget (71.6% utilization)                  │
│                                                                           │
├─ ANALYZE (post-execution) ────────────────────────────────────────────────┤
│                                                                           │
│  Actual input tokens:   6234  (estimate error: +2.4%)                     │
│  Cache read:            1880  (marker[0]+[1] hit)                         │
│  Cache creation:           0  (full cache hit!)                           │
│  Cache hit ratio:       100%  (session avg: 87%)                          │
│  Output tokens:          520  (estimated: 480, error: +8.3%)              │
│  Response latency:      1.2s                                              │
│                                                                           │
│  Feedback: cache_hit_ratio trending up (87% → 88% session avg)            │
│                                                                           │
└───────────────────────────────────────────────────────────────────────────┘
```

### 3.6 Mapping to Existing Astra Code

The design consolidates existing modules rather than replacing them. Each current module maps to a pipeline phase:

```
Phase           Existing Module                              Change Required
─────────────────────────────────────────────────────────────────────────────
Plan            AdaptiveCompactConfig::from_pressure()       → Keep; feed with predictive pressure
                CompactionTier                               → Add/centralize a tier selector helper
                TokenBudget::allocate()                      → NEW; derived from model limits + stats
                ProviderCacheStrategy                        → Wrap with ProviderCachePolicy

Bind            runtime/src/prompts/system.rs sections       → Each section becomes a bind_*() method
                chat_turn_base_payload()                     → bind_runtime_identity() + bind_tools()
                merge_active_skills_into_edge_profile()      → bind_skills()
                MemoriaClient::retrieve()                    → bind_memory()

Optimize        compact_tool_results_adaptive*()             → optimize() calls existing functions
                ForkPrefix (cache alignment)                 → build_anthropic_blocks() / build_prefix_blocks()
                tool_result_storage (spill)                  → spill_oversized() wraps existing persist logic

Execute         execute_turn_and_ingest_phase()              → execute() wraps existing API call
                normalizeMessages (implicit in payload)      → serialize_request()

Feedback        cloud_cache_diagnostics                      → ContextFeedback::from_response()
                ContextAssemblyTrace                         → Emitted by optimize(); enriched by feedback
                (NEW) PipelineStats                          → New: cross-turn statistics accumulation

Mid-turn        LiquidTactical                               → Orthogonal; runs between pipeline invocations
```

### 3.7 Shadow Pipeline (Mandatory Rollout Strategy)

Any change to context assembly risks breaking prompt cache alignment. A one-byte diff in the system prompt prefix can cause a full KV cache miss — visible as a sudden spike in `cache_creation_tokens` and latency. This makes "big bang" cutover unacceptable.

**The shadow pipeline pattern**: run the new pipeline *in parallel* with the existing assembly path, compare outputs, and only switch traffic once the diff is zero (or explicitly accepted).

```
                 ┌──────────────────┐
  ContextSources │  Old path        │──→ actual API request
        │        │  (existing code) │
        │        └──────────────────┘
        │
        ├──────→ ┌──────────────────┐
                 │  New pipeline     │──→ shadow output (not sent)
                 │  (Plan→Bind→Opt) │
                 └────────┬─────────┘
                          │
                          ▼
                 ┌──────────────────┐
                 │  Shadow diff     │──→ trace log: byte diff, token diff,
                 │                  │    cache scope diff, section order diff
                 └──────────────────┘
```

**Shadow diff checks** (run on every turn, zero cost to the LLM call):

| Check | What it catches |
|-------|----------------|
| System block byte hash | Any reordering, whitespace, or content change that would break prefix cache |
| Tool schema hash set | Tool additions/removals/description changes that affect cache key |
| Message count + role sequence | Missing or extra messages (e.g., emergent injection bug) |
| Cache marker positions | Breakpoint placement regressions |
| Total token estimate delta | Budget calculation drift (> 5% delta → warning) |

**Rollout sequence**:
1. Shadow-only: new pipeline runs, diff logged, old path serves traffic
2. Shadow-verified: diff is zero for N turns → emit trace confirmation
3. Flip: new pipeline serves, old path becomes shadow (catch regressions)
4. Retire: old path removed after M sessions with zero diff

The shadow pipeline is not optional — it is the mechanism that makes the optimizer safe to iterate on. Without it, any optimizer change requires full E2E regression testing before every deploy.

### 3.8 Implementation Plan

There are two viable implementation modes:

1. **Compatibility-first**: preserve the current request path, add trace/control-plane structure, then gradually enable behavior changes.
2. **Pipeline-first**: accept a large refactor and make `ContextPipeline` the canonical execution path immediately.

If the project is willing to break compatibility, choose **pipeline-first**. It is cleaner, removes adapter debt, and gives the optimizer real ownership of context semantics. The compatibility-first path remains useful as a rollout strategy, but it is not the best architecture if large changes are acceptable.

#### Recommended pipeline-first path

**Goal**: Replace scattered context assembly with a single pipeline-owned runtime path.

1. **Create the new state model first**: split `AgenticLoopState` into `AgentContext`, `SessionContext`, `SessionLatches`, `TurnState`, `ExternalSources`, `EmergentContext`, and `PipelineStats`. Keep execution-only state in a separate loop orchestrator, not in the context catalog.
2. **Move prompt section ownership into pipeline-compatible crates**: put `PromptSection`, `CacheScope`, section IDs, semantic-order groups, and provider cache policy in shared crates so `astra-turn-core` does not depend on `astra-runtime`.
3. **Make typed artifacts the internal representation**: replace string-first assembly with `SystemSectionArtifact`, `MessageArtifact`, `ToolSchemaArtifact`, `AttachmentArtifact`, `MemoryArtifact`, and `SpillReference`.
4. **Implement `plan → bind → optimize → serialize` as the only request builder**: remove parallel payload builders once the pipeline serializer covers main turns, subruns, skill runs, compaction, and forked agents.
5. **Make provider policy mandatory**: every provider must declare marker limits, cache scope support, message/content marker placement, thinking behavior, and tool schema serialization before it can execute through the pipeline.
6. **Make `EXPLAIN ANALYZE` always available**: trace is not a debug add-on; it is the audit log for every optimizer decision and every recovery path.
7. **Retire compatibility shims early**: once main loop and subrun paths use the pipeline, delete old assembly entry points rather than keeping dual behavior.

**Deliverable**: a single canonical context runtime where all LLM calls use the same catalog, optimizer, serializer, feedback loop, and failure recovery matrix.

**Validation**: not byte-equivalence. Validate semantic invariants: provider accepts requests, tool-call/tool-result pairing survives, thinking blocks obey provider rules, resume/replay works, compaction recovery works, cache diagnostics explain cold turns, and end-to-end agent tasks complete.

**When this is better**: when the cost of maintaining two context paths is higher than the risk of a large refactor; when the target is a long-lived engine rather than incremental patching; when cache/memory/compaction correctness needs one source of truth.

#### Compatibility-first path

The design is also deliverable incrementally. These phases are intentionally observability-first; they produce trace data before any behavior-changing optimizer is enabled.

#### Phase A: Trace-first scaffold

**Goal**: Define pure pipeline data types and emit an enriched trace without changing execution behavior.

1. Add pure structs in `astra-turn-core`: pressure, reserves, provider cache policy, section hashes, typed trace records.
2. Add `ContextAssemblyTraceV2` fields that can wrap the current `ContextAssemblyTrace` output.
3. In `astra-runtime`, build a read-only adapter from `AgenticLoopState` to a `ContextCatalogSnapshot`.
4. Wire existing `execute_turn_and_ingest_phase()` to emit the trace around the current assembly path.

**Deliverable**: `EXPLAIN`/verbose trace shows section order, cache scope, token estimates, section hashes, tool schema hash, provider policy, and actual usage. No request bytes or model behavior change.

**Validation**: Existing tests pass; trace output is deterministic for a fixed input; estimated vs actual token error is reported rather than hidden.

#### Phase B: Pressure + feedback loop

**Goal**: Close the feedback loop, but only for diagnostics at first.

1. Add `PipelineStats` to session state, bucketed by provider/model/query-source/prefix-hash.
2. Implement `ContextFeedback::from_response()` using existing usage fields (`prompt`, `cache_read`, `cache_creation`, `completion`).
3. Track output/thinking/schema reserve percentiles, not only averages.
4. Integrate existing `cloud_cache_diagnostics` and `ForkPrefix` hash data into cache-break attribution.

**Deliverable**: Predictive pressure is visible in EXPLAIN, but current compaction behavior remains the default unless a config flag enables predictive tier selection.

**Validation**: Compare raw vs predictive pressure on recorded sessions. Required metrics: prompt-too-long rate, max-output recovery count, cache hit ratio, cache creation tokens, compaction frequency.

#### Phase C: ContextSources adapter

**Goal**: Make the data-source catalog real without dismantling `AgenticLoopState`.

1. Extract only context-relevant fields into `ContextSources`/`ContextCatalogSnapshot`.
2. Keep execution machinery (hooks, cancellation, messaging, harness, permissions workflow) outside the pipeline.
3. Model `SessionLatches`, `RecoveryState`, and `EmergentContext` explicitly, but define persistence/resume semantics before use.
4. Add unit tests for the adapter so every field has an owner and lifecycle.

**Deliverable**: A stable adapter layer that explains where context comes from and keeps the existing runtime loop intact.

**Validation**: Adapter tests cover current `AgenticLoopState` field clusters; no prompt bytes change when the adapter is unused for optimization.

#### Phase D: Bind/Optimize separation without semantic changes

**Goal**: Split existing assembly into named bind/optimize steps while preserving behavior.

1. Wrap current `build_system_prompt_sections*`, tool schema selection, memory retrieval, and compaction calls behind `bind_*()` methods.
2. Add typed artifacts for system sections, messages, tool schemas, attachments, and spills.
3. Implement `optimize()` as a no-op semantic-preserving pass that only records what it would do.
4. Add `EXPLAIN ANALYZE` by joining trace + post-response feedback.

**Deliverable**: Full pipeline-shaped code path exists, but default output remains byte-equivalent or semantically equivalent to the current request path.

**Validation**: Existing E2E tests pass; new integration test verifies trace completeness and message/tool invariants.

#### Phase E: Gated cache and pressure optimization

**Goal**: Enable behavior-changing optimization only where provider policy and trace data support it.

1. Add a runtime flag/config for predictive compaction and cache alignment.
2. Preserve semantic prompt order; only reorder sections inside explicit reorderable groups.
3. Use `ProviderCachePolicy` for marker count, marker location, global scope, cache references, and fork/skip-cache-write behavior.
4. Roll out one provider/query-source path at a time.

**Deliverable**: Optimizer improvements are measurable, reversible, and provider-specific.

**Validation**: Cache hit ratio must not regress; prompt-too-long and recovery loops should decrease; latency overhead for pure pipeline work should stay below the configured budget.

### 3.9 Non-Negotiable Invariants

The optimizer is allowed to improve cost and cache behavior only if it preserves these invariants:

| Invariant | Why it matters |
|-----------|----------------|
| Tool-call/tool-result pairing remains valid | Provider APIs reject orphaned or reordered tool result blocks. |
| Thinking/redacted-thinking blocks are preserved or stripped only by provider rules | Mutating protected reasoning blocks can cause signature errors. |
| System prompt semantic order is stable | Cache locality must not change instruction precedence. |
| Provider serialization is canonical before hashing | Cache diagnostics and `ForkPrefix` validation depend on byte-stable prefixes. |
| Spilled content has an explicit recovery path | Compaction must be lossy only when the chosen tier says so. |
| Emergent context has provenance and TTL | Resume, replay, and forked agents must not double-inject stale attachments. |

### 3.10 Failure and Recovery Matrix

| Failure | Pipeline response |
|---------|-------------------|
| `prompt_too_long` / context-window error | Escalate `RecoveryState`, compact harder, record failed estimate in stats. |
| `max_output_tokens` recovery | Increase output reserve bucket for the same provider/model/query source. |
| Cache hit drops to cold | Diff prompt/tool/model/latch hashes; record cache break reason before reordering anything. |
| Summarizer/compact call overflows | Strip media, drop oldest compactable rounds, then fall back to truncation only with trace evidence. |
| Memory retrieval fails or times out | Surface as missing memory in trace; do not silently invent an empty success if repo patterns expect diagnostics. |
| Spill read fails | Keep placeholder and emit explicit recovery diagnostic; do not claim full content is recoverable. |

### 3.11 Trace Alerting

The trace is not a display feature — it is a monitoring system. Every `ContextAssemblyTrace` should be evaluated against alert rules **on every turn**, not just when a human looks at `EXPLAIN`.

**Alert rules** (evaluated by the pipeline after feedback is recorded):

| Alert | Condition | Severity | Action |
|-------|-----------|----------|--------|
| **Cache cold start** | `cache_hit_ratio == 0` on turn > 1 | Warning | Diff prefix hashes (system, tools, latches) against previous turn; emit break attribution to trace. |
| **Cache regression** | `session_avg_cache_hit_ratio` drops > 10% over 3 turns | Warning | Check for latch flip, tool schema churn, or model switch. |
| **Pressure spike** | `pressure.raw` jumps > 0.15 in one turn | Info | Log token delta breakdown (which section grew). |
| **Predictive miss** | `abs(estimated - actual) / actual > 0.20` for input or output tokens | Warning | Widen estimator bucket or flag degenerate query source. |
| **Compaction cascade** | 2+ compaction events within 3 turns | Warning | Session is growing faster than compaction can shrink — likely an unbounded tool-result producer. |
| **Recovery loop** | `RecoveryState.consecutive_ptl_errors >= 2` | Error | Emergency: force AggressivePrune, log full trace, consider aborting the turn. |
| **Emergent overflow** | `EmergentContext` items hit cap on any list | Info | Indicates high tool activity — may need cap adjustment or selective filtering. |
| **Shadow diff** | Shadow pipeline output differs from production path | Error (during rollout) | Block optimizer change from promoting to production. |

```rust
pub struct TraceAlert {
    pub severity: AlertSeverity,  // Info | Warning | Error
    pub rule: &'static str,       // Machine-readable rule name
    pub message: String,          // Human-readable explanation
    pub turn: u32,
    pub evidence: Value,          // Structured data for debugging
}

pub enum AlertSeverity { Info, Warning, Error }

impl ContextPipeline {
    /// Evaluate alert rules against the completed trace and feedback.
    /// Called by the orchestrator after every Execute + Feedback cycle.
    /// Returns alerts to be logged, surfaced in UI, or escalated.
    pub fn evaluate_alerts(
        &self,
        trace: &ContextAssemblyTrace,
        feedback: &ContextFeedback,
        stats: &PipelineStats,
        recovery: &RecoveryState,
    ) -> Vec<TraceAlert> { .. }
}
```

**Alert routing**: Alerts are not exceptions — they don't stop the pipeline. They flow to:
1. **Trace log** (always) — every alert is part of the turn's trace record
2. **Session UI** (Warning+) — surfaced in the REPL or admin dashboard
3. **Telemetry** (Error) — sent to observability backend for aggregation
4. **Auto-recovery** (Error, specific rules) — Recovery loop alert triggers automatic escalation in the next Plan phase

The key principle: **the pipeline should never silently degrade**. If cache hit ratio drops, if compaction cascades, if estimates are wrong — the trace alert system makes it visible *on the turn it happens*, not days later when someone notices increased costs.

---

## Part IV: Future Directions

These are **not** in scope for the initial implementation. They are recorded here as the theoretical framework suggests them, and the pipeline abstraction makes them possible.

### 4.1 Information Density Tracking

Track which context sections are actually *referenced* by the model's output. If a 5K tool result is never referenced in subsequent reasoning, it has low information density and should be compacted earlier.

Implementation sketch: after each turn, scan the model's output for references to content from specific sections (tool result filenames, memory IDs, etc.). Update a per-section "reference rate" in `PipelineStats`.

This is the context-engineering analogue of a database's **index usage statistics** — if an index is never used by any query plan, it's a candidate for removal.

### 4.2 Adaptive Cache Breakpoint Placement

Instead of fixed scope-based breakpoints, learn optimal breakpoint positions from cache hit/miss feedback. If a certain section boundary consistently yields cache hits, strengthen that breakpoint. If it consistently misses, consider merging with the previous section.

This is the analogue of a database's **automatic index tuning** — the system observes query patterns and suggests index changes.

### 4.3 Cross-Agent Context Sharing (ForkPrefix evolution)

The `ForkPrefix` mechanism already enables parent→child cache sharing. The natural extension is peer-to-peer sharing: if two sibling agents have overlapping context (same project rules, same tool schemas), they could share a common prefix.

This is the analogue of a database's **shared buffer pool** — multiple queries benefit from the same cached pages.

### 4.4 Cost-Based Section Selection

With enough `PipelineStats` history, the system could learn a simple cost model: "including memory section X costs Y tokens but increases output quality by Z%." This would enable true cost-based optimization — include a section only if its expected benefit exceeds its token cost.

The challenge is measuring "output quality" — this requires user feedback signals (thumbs up/down, corrections, retries). The infrastructure for this exists in Memoria's feedback mechanism.

---

## Appendix A: Cross-System Survey Data

> The patterns in Part II were derived from surveying three production agent systems:
> **System A** (a first-party agent CLI, TypeScript), **astra-engine** (this project, Rust),
> and **System B** (an open-source agent CLI, TypeScript). This appendix preserves the
> raw comparison data for reference. Column names are anonymized per the systems' terms.

### A.1 Compaction Strategy Comparison

| Aspect | System A | Astra | System B |
|--------|-------------|-------|----------|
| **Tiers** | 4 layers (micro → snip → auto → reactive) | 4 tiers (Normal → TrimSchemas → CompactHistory → AggressivePrune) | 2 layers (prune → compact) |
| **Trigger** | Fixed thresholds (ceiling − 13K) | Continuous pressure (0.0–1.0) | Boolean overflow check |
| **Adaptive params** | No (fixed keep/budget) | Yes (keep_recent, token_budget scale with pressure) | No |
| **Provider-aware** | Partially (microcompact is provider-native) | Fully (CompactStrategy per provider) | No |
| **Spill to disk** | No (cleared = gone) | Yes (persist before clearing, recoverable) | Yes (truncated output to /tmp) |
| **LLM summary** | Forked agent | LLM call with PTL retry (drop rounds on failure) | Compaction agent |
| **Failure recovery** | Circuit breaker (3 failures) + reactive compact | PTL retry (drop oldest, retry) + fallback to truncation | None |

### A.2 Prompt Cache Strategy Comparison

| Aspect | System A | Astra | System B |
|--------|----------|-------|----------|
| **Protocol support** | Single provider (cache_control + scope) | Anthropic + OpenAI prefix + Bedrock | Multi-provider (ephemeral/default) |
| **Prefix splitting** | 3 modes based on tools/boundary/provider | `ForkPrefix` — frozen byte-identical snapshots with per-tool hash | 2-part array rejoin |
| **Break detection** | Full hash-based (system + tools + model + betas), 2K drop threshold | Per-tool SHA-256, provider affinity, thinking config in key | None |
| **Latching** | Session-stable (mode flags, overage, betas) | CacheMode::SkipWrite for forks | None |
| **Multi-agent** | Side queries share cache-safe params | ForkPrefix capture→resolve with validate_spawn | None |

### A.3 Memory Integration Comparison

| Aspect | System A | Astra | System B |
|--------|----------|-------|----------|
| **Backend** | Filesystem (project-scoped memory dir) | Memoria MCP (HTTP + circuit breaker) | None (plugin hooks only) |
| **Taxonomy** | Flat (user, feedback, project, reference) | 6-category (User/Feedback/Project/Reference/Lesson/Episode) + trust tiers | N/A |
| **Injection point** | Attachment messages (auto-discovered) | System prompt section (§4) + tool descriptions | Plugin transform hooks |
| **Token budget** | Implicit (index file < 200 lines) | Explicit memory budget in current config; future `TokenBudget` should unify it | N/A |
| **Lifecycle** | Manual (user + rules-file driven) | Intent detection + namespace mapping + circuit breaker | N/A |

---

## Appendix B: Glossary

| Term | Definition |
|------|-----------|
| **Cache break** | Event where a prompt change invalidates the provider's KV cache, causing full recomputation. |
| **Cache scope** | Lifetime hint for a prompt section: Global (cross-session), Session (within session), None (per-turn). |
| **Compaction tier** | Aggressiveness level for context reduction: Normal → TrimSchemas → CompactHistory → AggressivePrune. |
| **Context pressure** | Ratio of used tokens to effective input limit (0.0–1.0+). Drives compaction tier selection. Predictive pressure adds reserve estimates; raw pressure uses only current tokens. |
| **ContextReserves** | Token budget subtracted from context window before pressure computation: output (expected response), thinking (extended thinking), schema (tool growth headroom). |
| **ContextSources** | The structured catalog of all data that can flow into an LLM context, organized by lifecycle (8 tiers from Immutable to Feedback). |
| **Emergent context** | Context discovered during execution that wasn't predictable at Plan time. Guarded by TTL + dedup + cap. Flows from Execute(N) to Bind(N+1). |
| **EXPLAIN** | Diagnostic mode that shows the planned context assembly without executing the API call. |
| **EXPLAIN ANALYZE** | Diagnostic mode that shows both the plan and the actual execution metrics (cache hits, token usage). |
| **ForkPrefix** | Frozen byte-identical snapshot of a parent turn's cacheable request prefix for spawning child agents. |
| **Latch** | Session-stable flag that, once set, is never unset — prevents mid-session cache breaks from mode toggles. DB analogy: one-shot DDL. |
| **Microcompact** | Lightweight compaction that clears old tool results with minimal placeholders. |
| **OptimizeLimits** | Per-turn gate struct that controls which optimizations are allowed. Each transformation (reorder, clear, prune, spill, summarize, drop) has an independent boolean gate. |
| **PipelineStats** | Cross-turn accumulated statistics (cache hit rates, response token estimates, section token history) that feed back into the Plan phase. Bucketed by provider/model/query-source. |
| **PTL retry** | Prompt-too-long retry — drop the oldest API round and retry the LLM call. |
| **Shadow pipeline** | Mandatory rollout strategy: run old and new assembly paths in parallel, diff outputs, and only switch traffic once the diff is zero or explicitly accepted. |
| **Spill** | Persisting content to disk before clearing it from context, enabling later recovery. |
| **Trace alert** | Automated rule evaluated on every turn's trace. Detects cache regression, pressure spikes, prediction misses, compaction cascades, and recovery loops. Not display — monitoring. |

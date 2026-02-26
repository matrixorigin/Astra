# Prompt Lifecycle Architecture

> **Status**: Core Design — single source of truth for prompt generation, assembly, versioning, and introspection  
> **Last Updated**: 2026-02-25 (revised per review feedback)  
> **Related**: [agent-introspection.md](agent-introspection.md), [memory-and-context.md](memory-and-context.md), [edge-cloud-execution.md](edge-cloud-execution.md), [evaluation-and-evolution.md](evaluation-and-evolution.md)

---

## The Problem

The current codebase has two prompt paths that diverged during evolution:

| | Path A: `/chat` + RunEngine | Path B: `/chat/turn` + EdgeChatLoop |
|---|---|---|
| System prompt | DB `agents` table → versioned | Hardcoded string |
| Context enrichment | ContextManager (5 sections, budget-capped) | Simplified `_enrich_system_prompt` |
| Skill selection | SkillPipeline (semantic + budget) | Edge tools passed through, no selection |
| Cross-session memory | Continuity + Observer | Observer only |
| Scratchpad | ✅ | ❌ |
| Prompt versioning | PromptManager (DB, rollback, feedback) | None |
| Self-awareness | ❌ | ❌ |

Path B is the future (edge-cloud is the deployment model), but it's the weaker path. This document unifies them into a single prompt lifecycle that leverages the edge-cloud split as a **strength**, not a limitation.

---

## Core Insight: The Prompt Is a Materialized View

A prompt is not a template. It's a **materialized view** over distributed state:

```
prompt = materialize(
    identity     ← agents table (cloud, versioned)
    self_model   ← capabilities + learned strengths/weaknesses (cloud, evolving)
    project_ctx  ← rules, conventions, file structure (edge, local files)
    memory       ← episodic + semantic + procedural (cloud, MatrixOne)
    working_mem  ← scratchpad, active plan (cloud, session-scoped)
    history      ← conversation events (cloud, budget-capped)
    few_shot     ← high-rated examples (cloud, feedback-driven)
    skills       ← available tools (edge + cloud, merged)
)
```

Each source has a different **owner**, **freshness**, and **cost**. The architecture should respect this.

---

## 1. The Prompt Assembly Pipeline

### Design: Edge Contributes, Cloud Assembles

The edge knows things the cloud doesn't (local files, tools, working directory). The cloud knows things the edge doesn't (memory, history, skill catalog, user preferences). Neither should try to do the other's job.

```
┌─────────────────────────────────────────────────────────────┐
│  EDGE (first turn only)                                     │
│                                                             │
│  Collects local context:                                    │
│  ├── project_rules     (.mo-agent/rules.md, steering/*.md)  │
│  ├── edge_tools        (tool schemas from ToolRouter)       │
│  ├── edge_profile      (cwd, git branch, language detected) │
│  └── user_preferences  (.mo-agent/preferences.json)         │
│                                                             │
│  Sends as structured payload in first /chat/turn            │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│  CLOUD (PromptAssembler — single entry point)               │
│                                                             │
│  Receives edge context + loads cloud state:                 │
│                                                             │
│  §1 IDENTITY (stable, cacheable)                            │
│  │   Source: agents.agent_config.system_prompt               │
│  │   Versioned by PromptManager. Rollback-safe.             │
│  │                                                          │
│  §2 SELF-MODEL (stable per session, refreshed daily)        │
│  │   Source: agent profile + edge_tools + cloud skills      │
│  │          + learned strengths from procedural memory      │
│  │   "I have these tools, I'm good at X, I struggle with Y"│
│  │                                                          │
│  §3 PROJECT CONTEXT (stable per session)                    │
│  │   Source: project_rules from edge                        │
│  │          + edge_profile (cwd, branch, language)          │
│  │                                                          │
│  §4 MEMORY (semi-stable, changes across sessions)           │
│  │   Source: SessionContinuity (prior sessions, knowledge)  │
│  │          + Observer (behavioral patterns)                │
│  │          + FewShotRetriever (high-rated examples)        │
│  │                                                          │
│  §5 WORKING MEMORY (changes within session)                 │
│  │   Source: Scratchpad (active notes, plans, hypotheses)   │
│  │                                                          │
│  §6 HISTORY (changes every turn, budget-capped)             │
│  │   Source: conversation_events, budget-controlled          │
│  │                                                          │
│  §7 CONSTRAINTS (stable, always present)                    │
│      Rules, format requirements, behavioral boundaries      │
│                                                             │
│  Output: [{"role": "system", "content": assembled_prompt}]  │
│  Side-effect: context_snapshot persisted for audit           │
└─────────────────────────────────────────────────────────────┘
```

### Why This Order Matters: Prompt Caching

LLM providers (Anthropic, OpenAI) cache prompt prefixes. Sections that change less frequently should come first:

```
§1 Identity        — changes on agent update (weeks/months)     ← CACHE HIT
§2 Self-model      — changes on skill/tool change (days)        ← CACHE HIT
§3 Project context — changes on project switch (hours)          ← CACHE HIT
§4 Memory          — changes across sessions (hours)            ← CACHE MISS (acceptable)
§5 Working memory  — changes within session (minutes)           ← CACHE MISS
§6 History         — changes every turn (seconds)               ← CACHE MISS
§7 Constraints     — never changes                              ← but at end, so no cache benefit
```

Moving constraints to the end is deliberate: they're the "guardrails" that the LLM should see last (recency bias helps compliance). Identity comes first because it frames everything else.

### Token Budget Allocation

When total budget is constrained, sections have different compression priorities:

| Priority | Section | Default Allocation | Compression Strategy |
|---|---|---|---|
| **Fixed** | §1 Identity + §7 Constraints | 300 tokens | Never compress |
| **Fixed** | §2 Self-model | 400 tokens (cap) | Drop MCP/delegation details first |
| **Elastic** | §6 History | Max 50% of remaining | Truncate oldest turns |
| **Elastic** | §4 Memory | Max 30% of remaining | Reduce recall count, drop low-relevance |
| **Elastic** | §5 Working memory | Max 10% of remaining | Keep only active plan + latest 3 notes |
| **Elastic** | §3 Project context | Max 10% of remaining | Truncate long rules |
| **Bonus** | §4 Few-shot examples | From memory budget | Fewer examples (1 instead of 2) |

Compression order when budget exceeded:
1. Few-shot examples → reduce to 0
2. History → truncate to last 3 turns
3. Memory → reduce recall to 5 entries
4. Working memory → keep only active plan
5. Self-model "What I've Learned" → drop (available via `get_agent_info`)
6. Identity + Constraints → **never compress**

### The Self-Model Section (New, Core Innovation)

This is what makes the agent self-aware. It's assembled from multiple sources:

```python
## Self-Model

You are **{agent_name}**, a {agent_type} agent running on mo-agent-engine.

### Capabilities
- Local tools (via edge): {edge_tool_names}
- Cloud skills: {cloud_skill_names}
- MCP servers: {mcp_server_names or "none"}
- You can delegate to: {delegate_targets or "no other agents"}

### Boundaries
- Permissions: {permission_summary}
- You cannot: {explicit_limitations}
- Context window: {model_context_size} tokens

### What I've Learned About Myself
{procedural_memory_insights}
# e.g. "I tend to over-read files. Search first, then read targeted sections."
# e.g. "For Go code review, I achieve 85% user satisfaction. For Python async, 62%."

### Introspection
For detailed runtime state (token usage, memory contents, session stats),
use the `get_agent_info` tool.
```

The "What I've Learned" subsection is the breakthrough: it comes from **procedural memory** — the `SelfImprovingSelector`'s historical accuracy data and the `Observer`'s behavioral pattern extraction. The agent literally knows its own strengths and weaknesses, backed by data.

**Cold start**: New agents or agents with insufficient history (<50 interactions) have no procedural memory insights yet. For these, the self-model falls back to **baseline insights by agent type**:

| Agent Type | Baseline Insight |
|---|---|
| `specialist` | "I focus deeply on my domain but may need to delegate cross-domain questions." |
| `reviewer` | "I read and analyze code but don't modify files directly." |
| `orchestrator` | "I break down tasks and delegate to specialists rather than solving directly." |
| `default` | "I'm still learning about my strengths and weaknesses. I'll improve as we work together." |

Baselines are replaced by data-driven insights once sufficient interaction history accumulates (threshold: 50 interactions with feedback signals).

### Self-Model Refresh Triggers

The self-model section is assembled at session start and cached for the session. It refreshes on:

| Trigger | What Changes | Mechanism |
|---|---|---|
| **Session start** | Full rebuild | `PromptAssembler.assemble()` called on first turn |
| **MCP server connect/disconnect** | Capabilities list | `MCPBridge.set_on_tools_changed()` callback invalidates cached self-model; next turn rebuilds |
| **Skill registry update** | Cloud skills list | `GateTrigger.on_skill_change()` invalidates; next session picks up |
| **Procedural memory update** | "What I've Learned" | Updated by `SelfImprovingSelector` learning cycle; next session picks up |
| **Explicit refresh** | Full rebuild | Edge sends `refresh_self_model: true` in `/chat/turn` request (for mid-session tool changes) |

Mid-session changes (MCP server added) are handled by invalidation + rebuild on next turn, not by re-injecting the system prompt (which would break conversation continuity). The LLM sees the updated self-model in the next turn's system message.

**Consistency note**: If the LLM has already made a decision in the current turn based on the old self-model (e.g., "I don't have a database tool" when an MCP server just connected), the tool result for that turn should include a `_capabilities_updated` hint:

```json
{
    "tool_call_id": "tc_xxx",
    "name": "get_agent_info",
    "result": "...",
    "_meta": {"capabilities_updated": true, "added": ["db_query"], "removed": []}
}
```

This lets the LLM self-correct in the same agentic loop ("Actually, I now have a db_query tool — let me use it") rather than giving a stale answer. The edge detects MCP changes via `MCPBridge.set_on_tools_changed()` and attaches the hint to the next `tool_results` payload sent to cloud.

---

## 2. Unified Prompt Path

### The Problem with Two Paths

Path A and Path B should not exist as separate implementations. The fix is not "make Path B do everything Path A does." The fix is: **one assembler, two callers**.

```
                    ┌──────────────┐
                    │PromptAssembler│  ← single implementation
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              ▼                         ▼
    ChatLoop._build_messages()    chat_turn._build_turn_messages()
    (Path A: cloud-only)          (Path B: edge-cloud)
```

`PromptAssembler` is a pure function (no side effects except logging):

```
Input:
  agent_id          → loads identity from DB
  user_query        → drives context retrieval + skill selection
  session_id        → loads history, scratchpad, continuity
  user_id           → loads user knowledge, preferences
  edge_context      → project_rules, edge_tools, edge_profile (from edge, or None)
  
Output:
  AssembledPrompt:
    system_message: str           # The full system prompt
    tools_schema: list[dict]      # Merged edge + cloud tools
    snapshot_id: str              # Context snapshot for audit
    token_breakdown: dict         # Per-section token usage
    cache_prefix_tokens: int      # How many tokens are cacheable
```

Both paths call the same assembler. Path A passes `edge_context=None` (cloud-only mode, tools from SkillPipeline). Path B passes the edge context from the request.

---

## 3. Leveraging MatrixOne

### Prompt Versioning as Time Travel

Current `PromptManager` stores versions in `prompt_templates` with `is_active` flag and manual rollback. This works but doesn't leverage MatrixOne's native capabilities.

**Better**: Prompt versions are just rows. MatrixOne's snapshot/time-travel lets us query any historical prompt state without explicit version management:

```sql
-- What prompt was active when this decision was made?
SELECT pt.content
FROM prompt_templates pt
  {SNAPSHOT = (SELECT snapshot_id FROM context_snapshots WHERE event_id = :decision_event_id)}
WHERE pt.template_id = :template_id AND pt.is_active = 1;

-- Compare prompt content between two points in time
SELECT 'before' as label, content FROM prompt_templates
  {SNAPSHOT = 'before_prompt_v3'}
WHERE template_id = 'system_general' AND is_active = 1
UNION ALL
SELECT 'after', content FROM prompt_templates
  {SNAPSHOT = 'after_prompt_v3'}
WHERE template_id = 'system_general' AND is_active = 1;
```

This means: **every context snapshot automatically captures the exact prompt version**, without explicit foreign keys. The prompt that produced a decision is always recoverable via time travel on the snapshot timestamp.

### Prompt A/B Testing via Branching

MatrixOne's zero-copy branching enables prompt experimentation without data duplication:

```
main branch:  prompt_templates has system_general@v2 (active)
                │
                ├── experiment/prompt-v3 branch (zero-copy clone)
                │     prompt_templates has system_general@v3 (active)
                │     Run regression gate on this branch
                │     Compare quality scores: v2 vs v3
                │
                └── If gate passes → merge v3 to main
                    If gate fails → drop branch, zero cost
```

This is the existing `Sandbox` + `RegressionGate` flow, but applied specifically to prompts. The key insight: **prompt changes are data changes**, and MatrixOne treats data changes as first-class versionable operations.

### Context Snapshot as Materialized View

The `context_snapshot` persisted per decision is currently a JSON blob. With MatrixOne, it can be a **queryable materialized view**:

```sql
-- Which prompt sections contributed most tokens across all decisions today?
SELECT
    JSON_EXTRACT(snapshot, '$.token_breakdown.identity') as identity_tokens,
    JSON_EXTRACT(snapshot, '$.token_breakdown.memory') as memory_tokens,
    JSON_EXTRACT(snapshot, '$.token_breakdown.history') as history_tokens,
    AVG(JSON_EXTRACT(snapshot, '$.cache_prefix_tokens')) as avg_cached
FROM context_snapshots
WHERE created_at > NOW() - INTERVAL 1 DAY
GROUP BY 1, 2, 3;

-- Find decisions where self-model section was missing (pre-introspection)
SELECT event_id, created_at
FROM context_snapshots
WHERE JSON_EXTRACT(snapshot, '$.sections.self_model') IS NULL
  AND created_at > '2026-02-20';
```

This enables the **ContextBudgetTuner** (already designed) to make data-driven decisions about token allocation — not from hardcoded ratios, but from actual production usage patterns queried via HTAP.

---

## 4. Edge-Cloud Tool Merging

### Current Problem

Edge tools and cloud skills are two separate worlds:
- Edge sends `edge_tools` (OpenAI schemas) → cloud passes them through to LLM unchanged
- Cloud has `SkillPipeline` (semantic retrieval, budget control) → only for cloud skills
- The LLM sees both, but the platform doesn't know the relationship

### Design: Unified Tool Catalog with Edge/Cloud Annotations

```
┌─────────────────────────────────────────────────────────────┐
│  Unified Tool Catalog (per session)                         │
│                                                             │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐        │
│  │ Edge Tools   │  │ Cloud Skills│  │ MCP Tools   │        │
│  │ (from edge)  │  │ (from DB)   │  │ (from edge) │        │
│  │              │  │              │  │              │        │
│  │ read_file    │  │ knowledge   │  │ db_query     │        │
│  │ write_file   │  │ memory      │  │ slack_send   │        │
│  │ shell        │  │ session_hist│  │              │        │
│  │ git          │  │             │  │              │        │
│  │ grep         │  │             │  │              │        │
│  └──────┬───────┘  └──────┬──────┘  └──────┬───────┘       │
│         │                  │                │               │
│         └──────────────────┼────────────────┘               │
│                            ▼                                │
│              SkillPipeline.get_tools_schema()                │
│              (semantic retrieve from ALL tools,              │
│               budget-controlled, unified audit)              │
└─────────────────────────────────────────────────────────────┘
```

Key changes:
1. Edge tools are **registered into SkillPipeline** on first turn (not passed through)
2. SkillPipeline does semantic retrieval across **all** tools (edge + cloud + MCP)
3. Budget control applies to the **merged** set
4. Audit trail covers **all** tool selections, not just cloud skills

This means: when the user asks "fix the bug in auth.go", the skill selector considers `read_file` (edge), `knowledge_search` (cloud), and `db_query` (MCP) in the same semantic retrieval pass, and picks the best combination within budget.

### Edge Tool Metadata Enrichment

Edge tools currently send bare OpenAI schemas. Enrich them with metadata for better selection:

```python
# Edge sends on first turn:
{
    "edge_tools": [...],           # OpenAI schemas (existing)
    "edge_profile": {              # NEW
        "cwd": "/home/user/project",
        "git_branch": "fix/auth-bug",
        "languages": ["go", "python"],
        "project_type": "matrixone",  # detected from go.mod / package.json
    }
}
```

The cloud uses `edge_profile` to:
- Bias skill selection toward relevant tools (Go project → prioritize Go-aware skills)
- Inject project context into the self-model section
- Enable project-type-specific prompt templates

### Edge Context Validation

Edge-contributed context is untrusted input. A malicious or compromised edge could inject harmful content into `project_rules` or `edge_profile`. The cloud applies validation before assembly:

1. **Schema validation**: `edge_profile` must conform to expected structure (string fields, bounded lengths). Reject malformed payloads.
2. **Size limits**: `project_rules` capped at 4000 chars, `edge_profile` fields capped at 200 chars each. Truncate silently.
3. **Content audit**: `project_rules` is logged in the context snapshot for post-hoc review. Basic prompt injection detection is applied synchronously — reject or sanitize content matching known injection patterns:
   - Patterns: `"ignore previous instructions"`, `"you are now"`, `"system: "`, `"<|im_start|>"`, `"[INST]"`
   - Action: strip matched lines and log a warning (don't reject the entire payload — legitimate rules may trigger false positives)
   - This is a best-effort defense, not a guarantee. Sophisticated injections require the Trust and Safety system agent's async analysis.
4. **Edge tools schema validation**: Tool schemas must conform to OpenAI function calling format. Reject tools with missing `name` or `parameters` fields.

---

## 5. Prompt Evolution Closed Loop

### The Full Cycle

```
                    ┌──────────────────────┐
                    │  User Interaction     │
                    │  (edge-cloud loop)    │
                    └──────────┬───────────┘
                               │
                    ┌──────────▼───────────┐
                    │  Implicit Feedback    │
                    │  Mining               │
                    │  (Observer, signals)  │
                    └──────────┬───────────┘
                               │
                    ┌──────────▼───────────┐
                    │  Diagnosis            │
                    │  "Why did this fail?" │
                    │  (LLM on low-rated    │
                    │   cases + snapshots)  │
                    └──────────┬───────────┐
                               │           │
                    ┌──────────▼──────┐    │
                    │  Prompt Mutation │    │
                    │  (LLM generates │    │
                    │   improved ver.) │    │
                    └──────────┬──────┘    │
                               │           │
                    ┌──────────▼──────┐    │
                    │  Branch Test     │    │ MatrixOne
                    │  (zero-copy      │    │ branching
                    │   sandbox)       │    │
                    └──────────┬──────┘    │
                               │           │
                    ┌──────────▼──────┐    │
                    │  Regression Gate │    │
                    │  (replay past    │◄───┘ time-travel
                    │   cases on new   │      for replay
                    │   prompt)        │
                    └──────────┬──────┘
                               │
                    ┌──────────▼───────────┐
                    │  Gate Pass?           │
                    │  Yes → activate       │
                    │  No  → discard branch │
                    └──────────────────────┘
```

This already exists in your design (evaluation-and-evolution.md). The new contribution is:

1. **Context snapshots make diagnosis precise** — when a case fails, we can query the exact prompt (via time travel) that produced the failure, not just the current prompt
2. **Self-model section evolves too** — the "What I've Learned" subsection updates as procedural memory accumulates, making the agent progressively more self-aware
3. **Edge context is part of the snapshot** — project_rules, edge_profile are captured, so replay can reconstruct the full prompt even for edge-cloud sessions

---

## 6. State Management Summary

### What Lives Where

| State | Location | Lifetime | Sync |
|---|---|---|---|
| Agent identity (system_prompt) | Cloud DB `agents` | Permanent, versioned | Cloud → edge via assembled prompt |
| Prompt templates | Cloud DB `prompt_templates` | Permanent, versioned | Cloud-only |
| Self-model | Cloud, assembled at session start | Session (refreshed on tool change) | Cloud → LLM via system prompt |
| Project rules | Edge filesystem | Session (loaded once) | Edge → cloud on first turn |
| Edge tools | Edge `ToolRouter` | Process lifetime | Edge → cloud on first turn, cached |
| Edge profile | Edge (detected) | Session | Edge → cloud on first turn |
| Conversation history | Cloud `_session_cache` + DB | Session (memory) + permanent (DB) | Cloud-only |
| Context snapshots | Cloud DB `context_snapshots` | Permanent | Cloud-only |
| Scratchpad | Cloud DB (session-scoped) | Session | Cloud-only |
| Memory (episodic) | Cloud DB `conversation_events` | 90 days active, then compressed | Cloud-only |
| Memory (semantic) | Cloud DB `sk_knowledge_entries` | Permanent, confidence-decayed | Cloud-only |
| Memory (procedural) | Cloud DB `skills_registry` + learnings | Permanent, versioned | Cloud-only |
| Skill selection history | Cloud DB `skill_selection_events` | Permanent | Cloud-only |
| Feedback | Cloud DB `llm_feedback` | Permanent | Cloud-only |
| Permissions | Edge `PermissionManager` | Process lifetime | Edge-only |

### Edge Is Stateless Across Sessions

The edge contributes context but doesn't persist state across sessions. This is by design:
- User can switch machines → cloud has everything
- Multiple edges can connect to same cloud → no conflict
- Edge crash → no data loss (cloud is source of truth)

The only edge-local state that matters is `project_rules` and `edge_tools`, both of which are re-sent on every new session's first turn.

---

## 7. Implementation: PromptAssembler

### Interface

```python
class PromptAssembler:
    """Assemble the full system prompt from distributed state.
    
    Single entry point for both /chat (cloud-only) and /chat/turn (edge-cloud).
    Pure function over DB state + edge context. No side effects except snapshot persistence.
    """

    def __init__(self, db: Session):
        self.db = db
        self.prompts = PromptManager(db)
        self.continuity = SessionContinuity(db)
        self.observer = Observer(db)
        self.few_shot = FewShotRetriever(db)

    def assemble(
        self,
        agent_id: str,
        user_query: str,
        session_id: str,
        user_id: str,
        edge_context: EdgeContext | None = None,
    ) -> AssembledPrompt:
        ...

@dataclass
class EdgeContext:
    """Context contributed by the edge on first turn."""
    project_rules: str | None = None
    edge_tools: list[dict] = field(default_factory=list)
    edge_profile: dict = field(default_factory=dict)

@dataclass
class AssembledPrompt:
    """Result of prompt assembly."""
    system_message: str
    tools_schema: list[dict]
    snapshot_id: str
    token_breakdown: dict[str, int]   # per-section token counts
    cache_prefix_tokens: int          # tokens in stable prefix
    sections: dict[str, str]          # raw sections for audit
```

### Migration Path

Migration uses a feature flag for gradual rollout:

```python
# config/settings.py
use_unified_assembler: bool = False  # Toggle per environment
```

**Step 1**: Implement `PromptAssembler` alongside existing code. Feature-flagged off.

**Step 2**: Enable for Path B (`/chat/turn`) first — it's simpler and benefits most.
```python
# api/routers/chat.py
if settings.use_unified_assembler:
    assembled = PromptAssembler(db).assemble(agent_id, user_query, session_id, user_id, edge_context)
    llm_messages = [{"role": "system", "content": assembled.system_message}]
else:
    llm_messages = _build_turn_messages(...)  # legacy
```

**Step 3**: Enable for Path A (`ChatLoop._build_messages`). Verify parity via regression gate — replay historical sessions through both paths, compare outputs.

**Step 4**: Remove legacy code paths once regression gate confirms parity.

Each step is independently deployable and rollback-safe.

---

## 8. Open Questions (from Review)

| # | Question | Resolution |
|---|---|---|
| 1 | How to measure intent classification false positive/negative rates? | Log every `get_agent_info` call as `introspection_query` event. Sample 500+ queries over 4 weeks, compute rates with 95% CI. Escalate to Approach B if CI lower bound >10%. |
| 2 | Should Self-Awareness block compress when context is tight? | Yes. "What I've Learned" subsection compresses first (available via dynamic query). Core identity + capabilities never compress. See §1 Token Budget Allocation. |
| 3 | When capabilities change mid-session (MCP server added), how is static introspection updated? | Invalidation + rebuild on next turn. Plus `_capabilities_updated` hint in tool_results so LLM can self-correct within the same agentic loop. See Self-Model Refresh Triggers §1. |
| 4 | What validation should cloud apply to edge-contributed context? | Schema validation, size limits, basic prompt injection pattern detection (strip + warn), content audit logging. See Edge Context Validation §4. |
| 5 | When budget exceeded, which sections compress first? | Few-shot → History → Memory → Working memory → Self-model learned insights. Identity + Constraints never compress. See §1 Token Budget Allocation. |
| 6 | What happens for new agents with no procedural memory? | Cold start baseline insights by agent type (specialist/reviewer/orchestrator/default). Replaced by data-driven insights after 50 interactions. See Self-Model §1. |

---

## 9. Relationship to Other Design Documents

| Document | What Changes |
|---|---|
| [Agent Introspection](agent-introspection.md) | Self-model section is the static introspection mechanism. `get_agent_info` tool is the dynamic complement. |
| [Memory and Context](memory-and-context.md) | PromptAssembler replaces ad-hoc context injection. Memory layers map to prompt sections. |
| [Edge-Cloud Execution](edge-cloud-execution.md) | Edge contributes structured `EdgeContext` instead of raw strings. Cloud assembles. |
| [Skills and Tools](skills-and-tools.md) | Edge tools registered into SkillPipeline for unified selection. |
| [Evaluation and Evolution](evaluation-and-evolution.md) | Context snapshots capture full assembled prompt for replay. Prompt A/B via branching. |
| [Data Versioning](data-versioning.md) | Time travel enables prompt reconstruction at any historical point. Branching enables prompt experimentation. |
| [Trust and Safety](trust-and-safety.md) | Every assembled prompt is snapshotted. Hallucination firewall verifies against snapshot. |

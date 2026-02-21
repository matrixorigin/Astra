# Skills and Tools

> **Status**: Core Design — single source of truth for skill system and tool integration  
> **Last Updated**: 2026-02-14

---

## The Shift: From Functions to Modular Expertise

The industry is moving from "tools as function calls" to "skills as modular expertise packages." Anthropic's Agent Skills introduces three-tier progressive loading.

mo-agent-engine's skill system goes beyond industry standards by adding what no one else has: **versioning, auditability, side-effect isolation, and regression testing** — all backed by MatrixOne's time-travel and branching capabilities. MCP/A2A are supported as interop layers, not architectural drivers.

---

## 1. Skill Architecture

### What a Skill Is

A skill is a **versioned, declarative capability** with:

- **Identity**: name, version (semver), description
- **Requirements**: what it needs (repo type, permissions, parameters)
- **Side-effect profile**: read / write / destructive (see [Trust and Safety](trust-and-safety.md))
- **Progressive disclosure**: metadata → summary → full instructions
- **Execution logic**: the actual code
- **Audit trail**: every invocation recorded with version, params, result

### Execution Model

Skills are **always executed in-process** as function calls within the ChatLoop. This is the
same approach used by Claude Code, Cursor, LangChain, and CrewAI. No containerization or
subprocess isolation for individual tool calls.

Three execution paths in ChatLoop:
1. **Built-in Skill** → `AgentExecutor.execute_skill()` → `ToolMockingLayer.execute()` → `skill.execute()`
2. **MCP Tool** → `MCPBridge.call_tool()` → MCP server (separate process via stdio/HTTP)
3. **Scratchpad** → in-memory, no external call

Safety is NOT achieved through isolation, but through:
- `SideEffectCategory` (READ/WRITE/DESTRUCTIVE) → approval gates
- `ToolMockingLayer` → replay mode blocks destructive ops
- MCP tools → naturally process-isolated

For **heavy background workloads** (model training, data collection), see
[Deployment Architecture § Background Jobs](deployment-architecture.md#3-execution-model-tools-vs-background-jobs).
These are NOT skills — they are jobs submitted via `/jobs` API.

### Progressive Disclosure (Anthropic-Aligned)

Following Anthropic's Agent Skills pattern and RAG-MCP research (Gan & Sun, 2025), skills load in two tiers with **real token accounting** and **semantic retrieval**:

```
Tier 1: INDEX (always available, never in LLM context)
  Embedding vector of name + description + triggers
  Used by semantic retriever to find candidates — LLM never sees this tier.
  Cost: 0 prompt tokens (lives in vector index only)

Tier 2: FULL SCHEMA (injected only for LLM-selected skills, measured tokens)
  Complete OpenAI tool JSON schema (from Pydantic model or default)
  Includes: name, description, parameters, detailed_instructions, examples, edge_cases
  Token cost: measured per-skill via len(json) // 4
  
  Note: The schema's name + description fields serve as the "summary" tier
        for LLM candidate ranking. There is no separate Tier 2 LLM ranking pass —
        semantic retrieval filters candidates, then LLM sees full schemas and
        selects via native function calling in a single pass.
```

**Key design principles** (learned from industry):
- **Real token measurement, not constants**: Each skill's Tier 2 cost is computed from actual serialized size, not hardcoded estimates. Schema sizes vary 3-5× across skills.
- **Budget is a hard cap**: If a skill doesn't fit the remaining budget, it is **excluded entirely** — no empty stubs. An empty-parameter stub wastes tokens and confuses the LLM.
- **Semantic retrieval is mandatory at scale**: RAG-MCP (arXiv:2505.03275) empirically shows keyword matching collapses beyond ~30 tools. Embedding-based retrieval achieves 3.2× accuracy improvement.
- **Semantic retrieval replaces LLM ranking**: The vector index does the candidate filtering (zero LLM cost). The LLM only sees budget-capped full schemas and selects via native function calling in a single pass.

**Why this matters**: With 50+ skills, putting all details in context wastes attention budget. Tier 1 embeddings let the retriever find candidates without any prompt tokens. Tier 2 full schemas are loaded only for budget-available skills, and the LLM selects directly.

### Skill Versioning

```
Register → Store in skills_registry (with version, code_hash, git_commit_hash)
         → Keep active version in memory
         → Archive old versions (never delete)

Execute  → Record skill_name + skill_version in conversation_events
         → Result logged with full provenance

Replay   → Load exact version from event metadata
         → Execute with historical skill logic
         → Reproduce exact behavior

Upgrade  → New version triggers regression gate (see trust-and-safety.md)
         → Gate passes → activate new version
         → Gate fails → reject, keep old version
```

---

## 2. MCP / A2A Compatibility Layer

> **MCP Status**: ✅ Implemented — `core/skills/mcp_bridge.py` (`MCPBridge` class), supports stdio + streamable HTTP transports, namespaced tool names, integrated into ChatLoop (3 execution paths). 12 unit tests passing.

MCP and A2A are interoperability protocols — useful for ecosystem integration, but not architectural drivers. Our Skill abstraction is strictly more powerful (versioned, auditable, sandboxable). MCP/A2A are thin adapters on top.

### Why Support MCP (But Not Center On It)

- **Ecosystem access**: Connect to external MCP servers (filesystem, databases, SaaS tools) without writing custom skills
- **Exposure**: Let external agents (Claude Code, Cursor) call our skills
- **Not a differentiator**: MCP is a wire format. Our value is in what happens around the call — audit, versioning, isolation

### Integration Design

```
┌─────────────────────────────────────────────────────────────┐
│  mo-agent-engine Skill Registry                             │
│                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│  │ Built-in     │  │ User-defined │  │ MCP Server   │     │
│  │ Skills       │  │ Skills       │  │ Adapters     │     │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘     │
│         │                  │                  │             │
│         └──────────────────┼──────────────────┘             │
│                            │                                │
│              Unified Skill Interface                        │
│              (name, version, params, execute)               │
└────────────────────────────┬────────────────────────────────┘
                             │
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
        MCP Client      MCP Client     Direct Call
        (external       (external      (built-in
         server A)       server B)      execution)
```

**Two directions**:

1. **MCP Client**: mo-agent-engine connects to external MCP servers. Each MCP tool is registered as a skill with auto-generated metadata, side-effect profile defaults to "write" (conservative).

2. **MCP Server**: mo-agent-engine exposes its skills as MCP tools. External agents (Claude Code, Cursor, etc.) can use our skills with full audit trail.

### A2A Gateway (Future)

Google's Agent-to-Agent protocol enables cross-platform agent collaboration. Our event blackboard coordination model maps naturally to A2A:

- **Agent Card** → AgentProfile (system_prompt, skills, model)
- **Task** → Delegation event with causal_chain_id
- **Message** → conversation_events with agent_id
- **Artifact** → Skill execution results

This is a future capability, but the architecture should not preclude it.

---

## 3. Skill Selection

### The Problem

With 50+ skills, the LLM can't efficiently choose from a flat list. Selection must be fast, accurate, and auditable. Research shows keyword matching collapses beyond ~30 tools (RAG-MCP, 2025). Semantic retrieval is mandatory, not optional.

### Multi-Stage Selection (Retrieval → Budget-Controlled Load)

```
Stage 1: RETRIEVE (semantic vector search, <50ms, 0 prompt tokens)
  - Encode query into embedding vector
  - Cosine similarity against skill embedding index (SkillIndex)
  - Return top-k candidates (k = 2× max_candidates for headroom)
  - Fallback: keyword matching if vector index unavailable

Stage 2: LOAD (Tier 3 full schema, budget-controlled)
  - Build full OpenAI tool schema for each candidate
  - Measure real token cost per schema: len(json) // 4
  - Include only if within remaining context_budget
  - Skills that exceed budget are excluded entirely (no stubs)
  - LLM selects + extracts parameters in a single function-calling pass
```

**Why not a separate "Tier 2 ranking" LLM call?** OpenAI-style function calling requires full parameter schemas to generate valid calls. A two-pass approach (rank with summaries → load full schemas) would double LLM latency for marginal benefit. Instead, the semantic retrieval stage (zero LLM cost) does the heavy filtering, and the budget cap ensures only a controlled number of full schemas reach the LLM. This matches Anthropic's actual implementation: the meta-tool decides which skill to load (cheap), then the full skill content is injected (expensive but targeted).

**Scaling behavior**:
| Skill count | Stage 1 method | Prompt tokens (5 candidates) |
|-------------|---------------|------------------------------|
| <20         | keyword OK    | ~500-2000 (budget-capped)    |
| 20-100      | semantic required | ~500-2000 (budget-capped)|
| 100+        | semantic + hierarchical | ~500-2000 (budget-capped) |

Note: prompt token cost stays **constant** regardless of total skill count — only the retrieval index grows.

### Auditable Selection

Every selection is recorded:

```json
{
  "event_id": "sel_01...",
  "session_id": "sess_01...",
  "user_query": "Review PR #123",
  "selected_skills": ["code_review", "summarize_pr"],
  "selection_method": "semantic",
  "created_at": "2026-02-20T12:00:00Z"
}
```

### Self-Improving Selection

The `SelfImprovingSelector` learns from historical failures:

```
Observe: skill selection led to poor quality_score
  → Analyze: was the wrong skill selected? Were parameters wrong?
  → Learn: update selection patterns (stored in procedural memory)
  → Validate: replay failing cases in sandbox with updated patterns
  → Deploy: if improvement confirmed, update selection weights
```

---

## 4. Tool Design Principles

Following Anthropic's guidance on writing tools for agents:

### 1. Token-Efficient Returns

```python
# Bad: return entire PR object (2000 tokens)
def get_pr(pr_number):
    return github.get_pr(pr_number)  # Everything

# Good: return only what's needed (200 tokens)
def get_pr(pr_number, fields=["title", "body", "files_changed", "status"]):
    pr = github.get_pr(pr_number)
    return {f: getattr(pr, f) for f in fields}
```

### 2. Self-Contained and Robust

Each tool handles its own errors, retries, and edge cases. The agent should never see raw HTTP errors or stack traces.

### 3. Minimal Overlap

If a human engineer can't definitively say which tool to use in a given situation, the agent can't either. Merge overlapping tools or add clear disambiguation in descriptions.

### 4. Descriptive Parameters

Parameter names and descriptions are part of the tool's "prompt." They must be unambiguous:

```python
# Bad
{"file": "string"}

# Good  
{"file_path": "Absolute path to the file to read, e.g. /src/auth/login.py"}
```

---

## 5. Skill Types and Lifecycle

### Types

| Type | Side Effects | Approval | Examples |
|------|-------------|----------|----------|
| **Read** | None | Auto | code_read, ci_status, search_code |
| **Write** | External state change | Configurable | create_pr, merge_pr, create_issue |
| **Destructive** | Irreversible change | Always required | delete_repo, force_push |
| **Compute** | Internal only | Auto | summarize, analyze, generate_tests |

### Lifecycle

```
Draft → Registered → Active → Deprecated → Archived

Draft:       Development, not available to agents
Registered:  In registry, not yet active (pending gate)
Active:      Available to agents, regression-tested
Deprecated:  Still works, but new selections discouraged
Archived:    Read-only, available for replay only
```

### Skill as MCP-Compatible Package

```yaml
# skill.yaml — declarative skill definition
name: code_review
version: 2.1.0
description: Review code changes for quality, security, and style

triggers:
  keywords: [review, PR, pull request, code quality]
  
requirements:
  repo_type: code
  access: read
  
parameters:
  pr_number:
    type: integer
    description: Pull request number to review
    required: true
  focus_areas:
    type: array
    items: {type: string, enum: [quality, security, performance, style]}
    description: Areas to focus the review on
    default: [quality, security]

side_effects:
  category: read
  external_apis: [github]
  
progressive_disclosure:
  tier1_tokens: 25
  tier2_tokens: 120
  tier3_tokens: 500

mcp_compatible: true
```

---

## 6. Skill Marketplace

### The Vision: App Store for Agent Skills

Skills should be publishable, discoverable, and subscribable — like an app store. A team builds a "Kubernetes Deployment" skill, publishes it, and any agent on the platform can subscribe and use it.

### Architecture: Publish → Subscribe → Use

MatrixOne's cross-tenant Publication is the natural mechanism: a skill publisher (account) publishes a database containing skill definitions, and subscriber accounts create read-only subscription databases to access them. Updates propagate automatically.

```
PUBLISHER (skill author account)
  │
  ├── Develops skill, tests in sandbox (CREATE CLONE → replay gate)
  │
  ├── Publishes:
  │     CREATE PUBLICATION skill_catalog_pub
  │       DATABASE skill_catalog TABLE skill_listings, skill_code;
  │     -- Subscribers specified: ALL or specific accounts
  │
  └── Updates skill → subscribers see changes immediately (zero-copy)

SUBSCRIBER (consumer account)
  │
  ├── Subscribes:
  │     CREATE DATABASE marketplace FROM publisher_acct
  │       PUBLICATION skill_catalog_pub;
  │     -- Read-only access to skill definitions
  │
  ├── Agent loads skill definition from subscription DB
  ├── Executes skill in own account context (isolation)
  └── Invocation logged in own conversation_events (audit)
```

**Why this works naturally**:
- Publisher updates a skill → all subscribers see the update instantly, no sync job
- Subscriber has read-only access → can't tamper with skill definitions
- Each subscriber runs skills in their own account → data isolation guaranteed
- Skill code + metadata live in the same database → no separate registry to maintain

This is fundamentally different from an "app store API" — it's **data-level distribution**. The marketplace IS the database. No API layer, no download step, no version sync problem.

### Skill Publishing Flow

```
Author develops skill
  │
  ▼
Local testing (sandbox)
  │
  ▼
Submit to marketplace
  │
  ▼
Automated validation:
  - skill.yaml schema valid?
  - Side-effect profile declared?
  - Test coverage provided?
  - Regression gate passes?
  │
  ▼
Published (status: "listed")
  │
  ▼
Other teams subscribe → skill available in their agents
```

### Marketplace Tables

```sql
CREATE TABLE skill_listings (
  listing_id    VARCHAR(64) PRIMARY KEY,
  skill_id      VARCHAR(64) NOT NULL,
  publisher_id  VARCHAR(64) NOT NULL,  -- account/team
  
  -- Discovery
  display_name  VARCHAR(255),
  description   TEXT,
  category      VARCHAR(100),          -- "devops", "security", "data", ...
  tags          JSON,
  
  -- Quality signals
  avg_rating    DECIMAL(3,2),
  total_installs INT DEFAULT 0,
  quality_gate_passed BOOLEAN DEFAULT FALSE,
  
  -- Access control
  visibility    VARCHAR(20) DEFAULT 'public',  -- public | team | private
  pricing_tier  VARCHAR(20) DEFAULT 'free',    -- free | premium
  
  -- Versioning
  latest_version VARCHAR(20),
  
  created_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Row-level access: premium skills only visible to paying subscribers
-- Uses MatrixOne's account-level isolation
```

### Subscription Model

```sql
CREATE TABLE skill_subscriptions (
  subscription_id VARCHAR(64) PRIMARY KEY,
  subscriber_id   VARCHAR(64) NOT NULL,  -- account/team subscribing
  listing_id      VARCHAR(64) NOT NULL,
  
  -- Version pinning
  pinned_version  VARCHAR(20),           -- NULL = auto-update to latest
  
  -- Usage tracking
  invocation_count INT DEFAULT 0,
  last_used_at    TIMESTAMP,
  
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

### Version Pinning via Clone

Subscribers who need version stability can clone instead of subscribing:

```sql
-- Pin to current version: clone the published skill (snapshot in time)
CREATE CLONE pinned_skills FROM publisher_acct.skill_catalog;
-- This is a writable copy — won't change when publisher updates

-- Or subscribe for auto-updates (read-only, always latest)
CREATE DATABASE live_skills FROM publisher_acct PUBLICATION skill_catalog_pub;
```

Two consumption modes from one mechanism: **subscribe** for always-latest, **clone** for pinned versions. No version management API needed.

### Why MatrixOne Makes This Trivial

In a traditional architecture, a skill marketplace requires:
- A registry service (API + database)
- A distribution mechanism (download, sync, CDN)
- Version management (semver, pinning, rollback)
- Access control (auth, entitlements, metering)
- Update propagation (webhooks, polling, push)

With MatrixOne: **Publication = distribution + auto-update. Clone = version pinning. Multi-Account = access control.** The entire marketplace infrastructure collapses into 3 SQL statements.

---

## References

- [Anthropic: Equipping Agents with Agent Skills](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills)
- [Anthropic: Writing Tools for AI Agents](https://www.anthropic.com/engineering/writing-tools-for-agents)
- [RAG-MCP: Mitigating Prompt Bloat in LLM Tool Selection (arXiv:2505.03275)](https://arxiv.org/abs/2505.03275) — 3.2× accuracy, ~50% token reduction via retrieval-based tool selection
- [Progressive Context Enrichment for LLMs (Inferable, 2025)](https://www.inferable.ai/blog/posts/llm-progressive-context-encrichment) — production validation of fetch-on-demand pattern
- [Claude Skills: Breaking LLM Memory Barriers (Developers Digest)](https://www.developersdigest.tech/blog/claude-skills-breaking-llm-memory-barriers) — ~30-50 tokens per skill until activation
- [MCP Specification](https://modelcontextprotocol.io/docs/getting-started/intro)
- [A2A Protocol Guide](https://a2aprotocol.ai/blog/2025-full-guide-a2a-protocol)
- [AI Agents 2026: Practical Architecture](https://www.andriifurmanets.com/blogs/ai-agents-2026-practical-architecture-tools-memory-evals-guardrails)
- [Vercel: Agent Skills — Creating, Installing, and Sharing](https://vercel.com/kb/guide/agent-skills-creating-installing-and-sharing-reusable-agent-context)

Content was rephrased for compliance with licensing restrictions.

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

### Progressive Disclosure (Anthropic-Aligned)

Following Anthropic's Agent Skills pattern, skills load in three tiers:

```
Tier 1: METADATA (always in context, ~20 tokens per skill)
  name: "code_review"
  trigger_keywords: ["review", "PR", "pull request", "code quality"]
  category: "github.pr_management"
  cost_estimate: "medium"

Tier 2: SUMMARY (loaded when skill is a candidate, ~100 tokens)
  description: "Review code changes in a pull request for quality, security, and style"
  parameters: {pr_number: int, focus_areas: list[str]}
  requirements: {repo_type: "code", access: "read"}

Tier 3: FULL (loaded when skill is selected for execution)
  detailed_instructions: "When reviewing code, check for..."
  examples: [{input: ..., output: ...}]
  edge_cases: [...]
  output_format: {...}
```

**Why this matters**: With 50+ skills, putting all details in context wastes attention budget. Tier 1 metadata lets the LLM identify candidates. Tier 2 summaries let it choose. Tier 3 details are loaded only for the selected skill.

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

With 50+ skills, the LLM can't efficiently choose from a flat list. Selection must be fast, accurate, and auditable.

### Multi-Stage Selection

```
Stage 1: FILTER (rule-based, <1ms)
  - Match skill trigger_keywords against query
  - Filter by repo type and permissions
  - Result: 5-10 candidates from 50+ skills

Stage 2: SELECT (LLM function calling)
  - Present candidates as tool definitions (Tier 2 summaries)
  - LLM selects via native function calling / structured output
  - Result: 1-3 skills to execute

Stage 3: LOAD (Tier 3 details for selected skills only)
  - Full instructions, examples, edge cases
  - Injected into context for execution
```

### Auditable Selection

Every selection is recorded:

```json
{
  "selection_event_id": "sel_01...",
  "query": "Review PR #123",
  "candidates": [
    {"skill": "code_review@1.2.0", "score": 0.95, "reason": "keyword_match: review, PR"},
    {"skill": "summarize_pr@1.0.0", "score": 0.72, "reason": "keyword_match: PR"}
  ],
  "selected": ["code_review@1.2.0"],
  "selection_method": "llm_function_calling",
  "filter_method": "keyword"
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
- [MCP Specification](https://modelcontextprotocol.io/docs/getting-started/intro)
- [A2A Protocol Guide](https://a2aprotocol.ai/blog/2025-full-guide-a2a-protocol)
- [AI Agents 2026: Practical Architecture](https://www.andriifurmanets.com/blogs/ai-agents-2026-practical-architecture-tools-memory-evals-guardrails)

- [Vercel: Agent Skills — Creating, Installing, and Sharing](https://vercel.com/kb/guide/agent-skills-creating-installing-and-sharing-reusable-agent-context)
- [SpoonOS: Web3-Native Skills Marketplace](https://neonewstoday.com/ai/spoonos-launches-web3-native-skills-marketplace-to-accelerate-composable-ai/)

Content was rephrased for compliance with licensing restrictions.

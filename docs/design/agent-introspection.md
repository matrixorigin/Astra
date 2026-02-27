# Agent Introspection

> **Status**: Core Design — single source of truth for agent self-awareness and meta-cognition  
> **Last Updated**: 2026-02-25 (revised per review feedback)  
> **Related**: [skills-and-tools.md](skills-and-tools.md), [memory-architecture.md](memory-architecture.md), [edge-cloud-execution.md](edge-cloud-execution.md), [agents-and-orchestration.md](agents-and-orchestration.md), [prompt-lifecycle.md](prompt-lifecycle.md)

---

## The Problem

```
user> 我有哪些 skill?
agent> 让我查看项目结构和文件…
       🔧 list_dir: .
       🔧 read_file: core/skills/registry.py
       …
```

The agent doesn't know it's being asked about **itself**. It treats "my skills" as a project code search instead of a self-referential query. This is not a retrieval failure — it's a **missing cognitive capability**: the agent has no model of itself.

This is the introspection problem. Without it, agents cannot:
- Answer "what can you do?" accurately
- Explain why they chose a particular tool
- Report their own limitations and boundaries
- Reason about their own state (context budget remaining, memory loaded, session history)

---

## Why This Matters Now

Three converging forces make introspection a first-class concern:

1. **Tool proliferation**. With 50+ skills, progressive disclosure means the agent doesn't even know its full catalog at any given moment. The skill selector picks tools for the LLM — but the LLM has no way to ask "what was available but not selected?"

2. **Edge-cloud split**. The agent's state is distributed: tools live on edge, memory lives on cloud, skill catalog lives in DB. No single component has the full picture. When a user asks "what do you know about me?", the answer requires assembling state from multiple locations.

3. **Multi-agent delegation**. When an orchestrator decides which specialist to delegate to, it needs to reason about each agent's capabilities. This is introspection of *other* agents — same mechanism, different target.

---

## 1. The Introspection Model

### What an Agent Should Know About Itself

Inspired by cognitive science's distinction between **cognition** (thinking about the world) and **metacognition** (thinking about one's own thinking), we define five introspection dimensions:

```
┌─────────────────────────────────────────────────────────────┐
│  CAPABILITY — "What can I do?"                              │
│  Available tools (edge + cloud), skills, MCP servers        │
│  Permissions (allow/ask/deny), side-effect boundaries       │
│  What I can NOT do (explicit limitations)                   │
├─────────────────────────────────────────────────────────────┤
│  STATE — "Where am I right now?"                            │
│  Current session, turn count, context budget remaining      │
│  Active plan (if any), pending tool calls                   │
│  Working directory, project context loaded                  │
├─────────────────────────────────────────────────────────────┤
│  MEMORY — "What do I remember?"                             │
│  Episodic: past interactions with this user                 │
│  Semantic: extracted knowledge, user preferences            │
│  Procedural: learned behaviors, skill selection patterns    │
│  What I've forgotten (decayed/archived)                     │
├─────────────────────────────────────────────────────────────┤
│  IDENTITY — "Who am I?"                                     │
│  Agent profile: name, role, system prompt summary           │
│  Model, temperature, behavioral constraints                 │
│  Delegation relationships (who I can call, who calls me)    │
├─────────────────────────────────────────────────────────────┤
│  CONFIDENCE — "How sure am I?"                              │
│  Uncertainty about current answer                           │
│  Historical accuracy on similar queries                     │
│  Knowledge freshness (last validation timestamps)           │
└─────────────────────────────────────────────────────────────┘
```

### Why Five Dimensions, Not Just "List My Tools"

A flat tool list answers one question. But users ask many kinds of self-referential questions:

| User Question | Dimension | What's Needed |
|---|---|---|
| "What tools do you have?" | Capability | Tool registry (edge + cloud) |
| "Can you run shell commands?" | Capability | Permission state |
| "What have we talked about?" | Memory | Session history + episodic memory |
| "Do you remember my preference for Go?" | Memory | Semantic knowledge search |
| "How many tokens have you used?" | State | Context budget tracker |
| "Why did you choose that approach?" | Confidence | Decision audit trail |
| "Who are you?" | Identity | Agent profile |
| "Can you ask the security reviewer?" | Identity | Delegation graph |

---

## 2. Design Principles

### Principle 1: Static Knowledge + Dynamic Query

Introspection has two layers:

**Static** — baked into the system prompt at session start. The LLM always knows the basics without any tool call:
- Agent name, role, high-level capabilities
- Behavioral boundaries (what it should refuse)
- Available tool categories (not full schemas — that's the skill selector's job)

**Dynamic** — queried at runtime via an introspection tool. For questions that depend on current state:
- Exact tool list with versions
- Context budget remaining
- Memory contents
- Session history stats

Why both? Static knowledge handles 80% of introspection questions with zero latency. Dynamic queries handle the remaining 20% that require runtime state. If we only had dynamic, the agent would need a tool call for "who are you?" — wasteful. If we only had static, the agent couldn't answer "how much context budget do I have left?" — stale.

### Principle 2: Introspection Is Not a Skill — It's a Platform Capability

Skills are domain capabilities (GitHub, knowledge search, code execution). Introspection is **meta** — it's about the platform itself. It belongs in the same category as context assembly and memory search: platform services that any agent can access.

In the edge-cloud architecture:
- **Edge introspection** (tools, permissions, working directory) → handled locally, no round-trip
- **Cloud introspection** (memory, skill catalog, session history, confidence) → handled by cloud API
- **Static introspection** (identity, role, boundaries) → in system prompt, no call needed

### Principle 3: Same Mechanism for Self and Others

When an orchestrator asks "what can the security-reviewer do?", it's the same introspection query with a different `target_agent_id`. This enables:
- Orchestrators reasoning about delegation targets
- Users asking "what agents are available?"
- System agents auditing other agents' capabilities

---

## 3. Architecture

### Where Introspection Fits

```
┌─────────────────────────────────────────────────────────────┐
│  System Prompt (static introspection)                       │
│  "You are {name}. You have access to: {tool_categories}.   │
│   You can/cannot: {boundaries}. Your model: {model}."      │
│                                                             │
│  Injected at session start by PromptManager.                │
│  Updated only when agent profile changes.                   │
├─────────────────────────────────────────────────────────────┤
│  Introspection Tool (dynamic introspection)                 │
│  get_agent_info(aspect, target_agent_id?)                   │
│                                                             │
│  Edge aspects: tools, permissions, cwd, project_rules       │
│  Cloud aspects: memory, skills, sessions, confidence        │
│  Always available. Not subject to skill selection budget.    │
├─────────────────────────────────────────────────────────────┤
│  Introspection in Context Assembly (implicit)               │
│  Cloud already injects memory, few-shot, skill index.       │
│  Add: agent_profile_summary in system prompt enrichment.    │
│  No new API — extend existing PromptManager.                │
└─────────────────────────────────────────────────────────────┘
```

### Edge-Cloud Split for Introspection

```
User asks: "What tools do you have?"
                    │
    LLM decides: call get_agent_info(aspect="capabilities")
                    │
        ┌───────────┴───────────┐
        ▼                       ▼
   Edge resolves:          Cloud resolves:
   - Local tools           - Cloud skills (from skill_registry)
     (file, shell,         - MCP servers registered
      git, search)         - Delegation targets
   - Permission state      - Skill selection history
   - Working directory
        │                       │
        └───────────┬───────────┘
                    ▼
            Merged response → LLM → natural language answer
```

**Implementation detail**: The edge already sends `tool_results` back to cloud in `/chat/turn`. Introspection follows the same path — cloud returns a `tool_call` for `get_agent_info`, edge resolves the local parts, sends result back. Cloud enriches with its own data before (or after) the edge response.

But for efficiency, we can split it:
- If `aspect` is edge-only (tools, permissions, cwd) → edge resolves entirely, no cloud round-trip
- If `aspect` is cloud-only (memory, skills, sessions) → cloud resolves in the `/chat/turn` processing
- If `aspect` is "all" → edge resolves local parts, cloud merges with cloud parts

---

## 4. The Introspection Tool

### Schema

```python
{
    "name": "get_agent_info",
    "description": "Query information about this agent or another agent. "
                   "Use when the user asks about capabilities, state, memory, "
                   "identity, or confidence — questions about the agent itself, "
                   "not about the project or codebase.",
    "parameters": {
        "aspect": {
            "type": "string",
            "enum": ["capabilities", "state", "memory", "identity", "confidence", "all"],
            "description": "Which dimension of agent information to query"
        },
        "target_agent_id": {
            "type": "string",
            "description": "Agent to query. Defaults to self. Use for delegation reasoning.",
            "default": "self"
        },
        "query": {
            "type": "string",
            "description": "Optional natural language query for memory/confidence aspects",
            "default": ""
        }
    }
}
```

### Response Structure by Aspect

**capabilities**:
```json
{
    "edge_tools": ["read_file", "write_file", "shell", "git_diff", "grep", "glob"],
    "cloud_skills": ["knowledge_search", "memory_recall", "session_history"],
    "mcp_servers": ["filesystem", "database"],
    "permissions": {"shell": "ask", "write_file": "ask", "read_file": "allow"},
    "limitations": ["Cannot access network directly", "Cannot modify files outside project root"]
}
```

**state**:
```json
{
    "session_id": "ses_abc123",
    "turn_number": 7,
    "context_budget": {"total": 128000, "used": 45000, "remaining": 83000},
    "active_plan": null,
    "working_directory": "/home/user/project",
    "project_rules_loaded": true
}
```

**memory**:
```json
{
    "episodic": {"session_count": 12, "total_turns": 156, "last_session": "2026-02-24"},
    "semantic": {"knowledge_entries": 34, "topics": ["auth", "database", "CI pipeline"]},
    "procedural": {"learned_patterns": 8, "skill_improvements": 3},
    "recent_topics": ["bug fix in auth.go", "CI pipeline optimization"]
}
```

**identity**:
```json
{
    "agent_id": "dev-agent",
    "name": "Development Assistant",
    "model": "claude-sonnet-4-20250514",
    "role_summary": "General-purpose development assistant for MatrixOne codebase",
    "can_delegate_to": ["security-reviewer", "perf-reviewer"],
    "delegated_from": ["orchestrator"]
}
```

### Why Not Multiple Tools?

One tool with an `aspect` parameter instead of five separate tools (`get_tools`, `get_memory`, `get_state`, etc.) because:
- Fewer tools in the schema = less token cost
- The LLM can ask for `"all"` in a single call
- Matches the mental model: "tell me about yourself" is one question, not five

---

## 5. Static Introspection: System Prompt Enrichment

The most impactful change is the simplest: **tell the agent who it is** in the system prompt.

### Current State

Seed agents have minimal self-description:
```
"You are a code implementation specialist. Write clean, well-tested code."
```

No mention of available tools, memory capabilities, delegation relationships, or boundaries.

### Proposed Enrichment

PromptManager assembles a `## Self-Awareness` section at session start:

```markdown
## Self-Awareness

You are **{agent_name}** ({agent_id}), a {agent_type} agent.

### Your Capabilities
- **Local tools**: file operations (read/write/search), shell commands, git operations
- **Cloud services**: memory search, knowledge base, session history
- **MCP servers**: {list or "none configured"}
- **Delegation**: You can delegate to {delegate_targets or "no other agents"}

### Your Boundaries
- You need user permission for: {ask_permissions}
- You cannot: {explicit_limitations}
- Your context window: {model_context_size} tokens

### Your Memory
- You have access to past conversations with this user
- You can recall learned knowledge and preferences
- Use `get_agent_info` to query your detailed state

When users ask about your capabilities, tools, memory, or identity,
answer from this knowledge. Use `get_agent_info` only for dynamic
runtime details (exact token usage, session stats, etc.).
```

This is injected by extending `PromptManager.get_system_prompt()` to merge the agent's base prompt with a generated self-awareness block. The block is built from:
- `AgentProfile` (identity, delegation)
- `ToolRouter.list_tools()` (edge tools, cached)
- `SkillRegistry` (cloud skills, cached)
- `PermissionManager` (boundaries)

Token cost: ~200-300 tokens for basic capabilities. With 50+ tools and complex delegation graphs, the full list could reach 500-800 tokens. To control this:

- **System prompt uses categories**, not full tool lists: "file operations (read/write/search), shell commands, git operations" — not individual tool names
- **Detailed tool list** is available via `get_agent_info(aspect="capabilities")` — only loaded on demand
- **Budget cap**: Self-awareness block is hard-capped at 400 tokens. If content exceeds this, compress by dropping MCP server details and delegation graph (available via dynamic query)

---

## 6. Intent Classification: Self vs. World

The demo3 failure shows the LLM can't distinguish "my skills" (self-referential) from "the project's skills" (world query). Two approaches, from simple to sophisticated:

### Approach A: Let the LLM Figure It Out (Recommended for V1)

With proper static introspection in the system prompt + `get_agent_info` tool available, modern LLMs (Claude, GPT-4) are good at recognizing self-referential queries. The system prompt enrichment + tool description is usually sufficient.

**Why this works**: The failure in demo3 happened because the agent had zero self-knowledge and no introspection tool. Given both, the LLM naturally prefers answering from its own knowledge over searching the codebase.

### Approach A+: Strengthen Tool Description (V1 Hardening)

To reduce misclassification risk without a separate router, add stronger trigger examples directly in the `get_agent_info` tool description:

```python
"description": "Query information about this agent or another agent. "
               "Use when the user asks about YOUR capabilities, state, memory, "
               "identity, or confidence — questions about the agent itself, "
               "not about the project or codebase.\n\n"
               "Trigger examples:\n"
               "- 'What tools do you have?' → aspect=capabilities\n"
               "- 'What are my skills?' (where 'my' = agent) → aspect=capabilities\n"
               "- 'Do you remember our last conversation?' → aspect=memory\n"
               "- 'How confident are you?' → aspect=confidence\n"
               "- 'Who are you?' → aspect=identity\n\n"
               "Do NOT use for questions about the project's code, files, or architecture.",
```

This costs ~50 extra tokens in the tool schema but significantly reduces ambiguity for edge cases like "what skills are available?"

### Approach B: Explicit Intent Router (Future, if A+ is insufficient)

If Approach A+ fails at scale, add a lightweight classification step:

```
User query → Intent classifier (rule-based, zero LLM cost)
  ├── INTROSPECTION → inject get_agent_info hint, bias toward self-knowledge
  ├── WORLD_QUERY   → normal skill selection pipeline
  └── AMBIGUOUS     → ask clarification or try both
```

Rule-based signals for introspection intent:
- Pronouns: "my", "your", "you", "I" (in agent context)
- Verbs: "can you", "do you have", "what are your", "tell me about yourself"
- Topics: "tools", "capabilities", "memory", "context", "skills" (when subject is agent)

**Measurement**: Track false positive/negative rates by logging introspection intent classification results (see §6.1) and comparing against actual tool calls made. Sample 500+ queries over 4 weeks, compute misclassification rate with 95% confidence interval. Escalate to Approach B if the lower bound of the CI exceeds 10% (i.e., we're statistically confident the true rate is above 10%, not just a noisy sample).

---

## 7. Research Context

### Industry

- **Anthropic Agent Skills** (2025.10): Three-tier progressive disclosure. Skills declare triggers, instructions, and tool schemas. The agent loads skills on demand. No explicit introspection mechanism — the agent "knows" its skills because they're in context. Our approach extends this with queryable runtime state.

- **Claude Code / Cursor**: Hardcoded tool lists in system prompt. No dynamic introspection. Works because the tool set is small and fixed. Breaks when tools are dynamic or numerous.

- **ElizaOS**: Plugin system with `manifest.yaml` declaring capabilities. Agents can query the plugin registry. Closest to our approach, but no edge-cloud split.

### Academic

- **MemSkill** (arXiv:2602.02474, 2026): Controller learns to select relevant skills; designer evolves the skill set by reviewing failures. The controller's skill selection is a form of implicit introspection — it must model what each skill can do. Our `get_agent_info` makes this explicit.

- **DeepAgent** (arXiv:2510.21618, 2025): Tool discovery as part of reasoning. The agent doesn't select from a fixed list — it discovers tools mid-thought. This requires deep introspection: the agent must know what tools exist, what they do, and whether they're relevant, all within the reasoning trace.

- **SkillRL** (arXiv:2602.08234, 2026): Hierarchical skill discovery via RL. Skills are discovered, composed, and refined through experience. The RL policy implicitly encodes a model of available skills — a learned form of introspection.

- **Observational Memory** (2025): Agents observe their own behavior patterns and form meta-cognitive insights. Directly relevant — our `core/memory/observer.py` implements this. Introspection extends it from passive observation to active querying.

---

## 8. Implementation Plan

### Phase 1: Static Introspection (Minimal, High Impact)

Extend `PromptManager` to inject a `## Self-Awareness` block into system prompts.

- Input: `AgentProfile` + edge tool list + cloud skill list + permissions
- Output: ~200 token self-description block appended to system prompt
- Edge change: `EdgeChatLoop` sends tool list in first `/chat/turn` (already sends `project_rules`)
- Cloud change: `PromptManager.get_system_prompt()` merges base prompt + self-awareness block

**This alone fixes the demo3 problem.** The LLM will know it has tools and can answer "what are my skills?" from its system prompt.

### Phase 2: Dynamic Introspection Tool

Add `get_agent_info` as a platform tool (always available, not subject to skill selection budget).

- Edge implementation: `cli/tools/introspection.py` — resolves local aspects
- Cloud implementation: extend `/chat/turn` to resolve cloud aspects
- Merge logic: edge resolves local, cloud enriches with DB state
- Available to all agents by default (platform capability, not a skill)
- **Basic confidence signals**: Include knowledge freshness timestamps and skill selection accuracy from `SelfImprovingSelector` history. Full confidence calibration remains Phase 4, but basic signals (last validation date, historical accuracy %) are available here.
- **Introspection audit**: Every `get_agent_info` call is logged as a `conversation_event` with `event_type=introspection_query`. This enables tracking introspection usage patterns and measuring intent classification accuracy.

### Phase 3: Cross-Agent Introspection

Enable `target_agent_id` parameter for orchestrator use cases.

- Query another agent's profile, capabilities, and historical performance
- Used by orchestrator for delegation decisions
- Used by system agents for capability auditing

### Phase 4: Learned Introspection (MemSkill-Aligned)

The agent learns its own strengths and weaknesses from historical data.

- "I'm good at Go code review but struggle with Python async patterns"
- "I tend to over-read files — I should search first"
- Stored as procedural memory, updated by `SelfImprovingSelector`
- Injected into self-awareness block as learned behavioral insights

---

## 9. Relationship to Existing Components

| Component | How Introspection Relates |
|---|---|
| `PromptManager` | Extended to inject self-awareness block (Phase 1) |
| `ToolRouter` (edge) | Provides tool list for capability introspection |
| `SkillRegistry` (cloud) | Provides skill catalog for capability introspection |
| `PermissionManager` | Provides boundary information |
| `AgentProfile` / `AgentRegistry` | Provides identity and delegation graph |
| `ContextManager` | Provides state (budget, turn count) |
| `memory/observer.py` | Provides learned behavioral patterns (Phase 4) |
| `SelfImprovingSelector` | Provides skill selection accuracy history (Phase 4) |
| `confidence_scorer.py` | Provides confidence dimension |
| `EdgeChatLoop` | Executes edge-side introspection tool calls |

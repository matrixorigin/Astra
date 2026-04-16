# Session Memory Protocol — Design Document

**Status**: Draft
**Author**: astra-engine team
**Date**: 2026-04-16
**References**: Claude Code compaction system analysis, Memoria API

## 1. Problem Statement

Context compaction drops the user's original task, causing the agent to lose track of what it was doing. Observed in session `f3ef23fe`: after proactive compression at 83% pressure, the agent said "The original user request was lost during context compaction (35 earlier messages removed)."

Root cause: `TieredCompaction` (Layer 3) preserves system messages + tail N turns, but the first user message (the original task) falls in the "middle" and gets deleted.

### Goals

1. **Goal preservation**: The user's original task must never be lost, under any compaction pressure
2. **Token efficiency**: Memory injection must be cache-friendly and pressure-adaptive, not a fixed tax
3. **Continuous high-quality memory**: Structured, governed session state that survives multiple compactions
4. **Deployment flexibility**: Works in CLI (edge), web agent (server), and Claude Code compatibility modes
5. **Prompt cache preservation**: Memory operations must not break the cached prefix

## 2. Architecture Overview

### 2.1 Four-Layer Memory Pyramid

```
         ┌─────────────────┐
    L0   │  Anchor + MC     │  Cost: zero LLM, every turn
         │  ~50t + savings  │  Anchor in system prompt, microcompact clears old tool results
         ├─────────────────┤
    L1   │  Session Memory  │  Cost: cheap model, periodic (every 5K tokens + 3 tool calls)
         │  ≤2000t inject   │  10-section structured notes, replaces LLM summary at compaction
         │  ≤4000t stored   │
         ├─────────────────┤
    L2   │  Compact Summary │  Cost: cheap model, only when L1 unavailable
         │  ≤3000t          │  9-section LLM summary with anti-drift safeguards
         ├─────────────────┤
    L3   │  Durable Memory  │  Cost: zero/low, session end
         │  not injected    │  Episodic, procedural, semantic — retrieved on demand via Memoria
         └─────────────────┘
```

### 2.2 Design Principles

Borrowed from Claude Code's production-proven patterns:

| Principle | Claude Code Practice | Our Adaptation |
|-----------|---------------------|----------------|
| Anti-drift | All user messages verbatim in summary + direct quotes for next step | L1 "User Messages" section + L0 anchor |
| Cache-aware | Forked agent shares cached prefix; never inject into stable prefix | Memory in CacheScope::None; injection after cache breakpoint |
| Layered compaction | Microcompact → session memory → LLM summary → context collapse | L0 MC → L1 session memory → L2 LLM summary |
| Size governance | Per-section 2K token cap + total 12K cap + auto-condensation warnings | Per-section budgets + total 2K inject / 4K stored |
| Structured template | 10 immutable section headers, only content below is editable | 10-section markdown with `[session-memory:v1]` prefix |
| Zero-LLM compaction | Session memory replaces LLM summary when available | L1 replaces L2 at compaction time |
| Post-compact restoration | Re-inject top 5 files, plans, skills, tool schemas | Same, with file dedup against preserved messages |
| Continuation prompt | "Pick up the last task as if the break never happened" | Same, injected after compaction |

## 3. L0: Anchor + Microcompact

### 3.1 Session Anchor

A single-line task summary embedded in the system prompt's dynamic section (`CacheScope::None`). Never deleted by any compaction layer.

**Format**:
```
[session-anchor] {task, ≤30 words}. Currently: {current_work, ≤15 words}. {done}/{total} steps.
```

**Example**:
```
[session-anchor] Add OAuth support to API with JWT tokens. Currently: token refresh in src/auth/refresh.rs. 3/5 steps.
```

**Lifecycle**:
- **Created**: Turn 1, rule-extracted from first user message (zero LLM)
- **Updated**: Every time L1 updates, L0 is re-derived from L1's Task + Current State sections (zero LLM)
- **Injected**: Appended to the dynamic system prompt section (after profile_desc), inside `CacheScope::None`
- **Compaction**: Treated as part of system prompt — never removed by TieredCompaction or ReactiveCompact
- **Cost**: ~50 tokens, constant

**Cache impact**: None. Placed in `CacheScope::None` (already non-cached dynamic section). Does not affect the Global/Session cached prefix.

**Rule extraction** (zero LLM):
```rust
/// Sections are parsed by matching `# {Section Name}` headers in the markdown.
/// Content is everything between the header and the next `# ` header (or EOF).
fn extract_anchor(first_user_msg: &str, l1: Option<&SessionMemory>) -> String {
    if let Some(l1) = l1 {
        // Derive from L1's parsed sections
        let task = first_sentence(l1.section("Task Specification"));
        let current = first_sentence(l1.section("Current State"));
        let (done, total) = count_progress_markers(l1.section("Progress"));
        format!("[session-anchor] {task}. Currently: {current}. {done}/{total} steps.")
    } else {
        // First turn: extract from user message
        let task = truncate_words(first_user_msg, 30);
        format!("[session-anchor] {task}. Currently: starting. 0/0 steps.")
    }
}
```

### 3.2 First User Message Preservation

In addition to the anchor, `TieredCompaction` and `ReactiveCompact` must preserve the first `role: "user"` message (the original task). This is a ~10-line change in each compressor's `compress()` method.

**Rationale**: The anchor is a compressed summary; the original message preserves the user's exact words, which is critical for anti-drift (Claude Code's Section 6 "All User Messages" pattern).

### 3.3 Adaptive Microcompact

Clears old tool result content for compactable tools, with pressure-adaptive retention.

**Compactable tools**: `read_file, grep, glob, git_show, git_log, web_search, web_fetch, code intel tools`
**Non-compactable**: `bash, write_file, str_replace, skill, delegate, memory_store` (side effects / non-idempotent)

**Adaptive retention** (existing logic, refined):

| Context Pressure | Keep Recent N | Token Budget per Result |
|-----------------|---------------|------------------------|
| < 60% | 6 | 12K |
| 60–75% | 4 | 8K |
| 75–90% | 2 | 4K |
| ≥ 90% | 1 | 2K |

**Duplicate read elimination**: When the same file path appears in multiple `read_file` results, only the most recent is kept. Earlier reads are replaced with `"[Earlier read of {path} — see later read for current content]"`.

**Cache impact**: Microcompact modifies message content in the volatile region (after the cache breakpoint). It does NOT affect the cached prefix. After microcompact, the cache breakpoint is re-placed on the last message.

## 4. L1: Session Memory

The core innovation layer. A structured, incrementally-updated session state stored in Memoria, used to replace LLM summarization during compaction.

### 4.1 Template (10 Sections)

```markdown
[session-memory:v1]
# Session Title
{5-10 word descriptive title}

# Task Specification
{What the user originally asked. Verbatim if short, paraphrased if long. IMMUTABLE unless user explicitly changes direction.}

# Current State
{What is actively being worked on RIGHT NOW. ALWAYS updated on every extraction. Include file names and specific details.}

# Key Files
{Files read/modified/created, one per line: path — brief purpose}

# Progress
{Checklist: ✅ done / 🔄 in progress / ⏳ pending}

# Errors & Corrections
{Errors encountered and fixes. User corrections have HIGHEST priority — never remove them.}

# Decisions
{Key technical decisions and rationale}

# User Messages
{ALL user messages verbatim (excluding tool results). Critical for anti-drift.}

# Worklog
{Terse step-by-step: turn N — what was attempted, what happened}

# Context
{Turn count, tokens used, pressure, last update timestamp}
```

### 4.2 Size Governance

Two versions of L1 exist: the **stored version** (full detail in Memoria) and the **injection version** (compressed for context injection).

**Stored version** (in Memoria, ≤4000 tokens):

| Section | Max Tokens | Condensation Priority |
|---------|-----------|----------------------|
| Session Title | 20 | Never condense |
| Task Specification | 200 | 🔴 Never condense |
| Current State | 400 | 🔴 Never condense |
| Key Files | 500 | 🟡 Drop oldest entries |
| Progress | 400 | 🟡 Drop completed items |
| Errors & Corrections | 500 | 🟡 Drop resolved errors (keep user corrections) |
| Decisions | 400 | 🟡 Drop oldest decisions |
| User Messages | 800 | 🔴 Never condense (truncate oldest if over budget) |
| Worklog | 700 | 🟢 First to condense |
| Context | 50 | Auto-generated |
| **Total** | **≤4000** (hard cap, enforced by update prompt) | |

When a section exceeds its budget, the update prompt includes: "CRITICAL: Section '{name}' exceeds {max} tokens. Condense oldest entries first. Prioritize keeping 'Current State', 'Task Specification', and 'User Messages' accurate."

**Injection version** (for context, ≤2000 tokens) — rule-compressed from stored version, zero LLM:

| Section | Injection Rule |
|---------|---------------|
| Task Specification | Full text |
| Current State | Full text |
| Key Files | File names only, no descriptions |
| Progress | Only 🔄 and ⏳ items |
| Errors & Corrections | Only unresolved errors + all user corrections |
| Decisions | Most recent 2 entries, ≤15 words each |
| User Messages | Last 3 user messages |
| Worklog | Omitted |
| Context | Omitted |

### 4.3 Update Trigger

Same dual-threshold as Claude Code's session memory:

```
Initial gate: context tokens ≥ 10,000 (skip for trivial sessions)

Subsequent updates require BOTH:
  - Token growth ≥ 5,000 since last extraction
  - Tool calls ≥ 3 since last extraction
  
OR:
  - Token growth ≥ 5,000 AND last assistant turn has no tool calls (natural break)
```

### 4.4 Update Mechanism

Uses a **cheap text model** (e.g., `glm-4-flash-250414`, `deepseek-chat`, or the cheapest available model in the model registry).

**Execution model**: Async fire-and-forget. The main turn does NOT block on L1 extraction. The result is available on the next turn. If compaction triggers while an L1 update is in-flight, wait up to 15 seconds for it to complete (borrowed from Claude Code's `waitForSessionMemoryExtraction()`). If timeout, fallback to L2.

**Update prompt**:

```
You are updating a structured session memory file based on new conversation messages.

Rules:
- ALWAYS update 'Current State' to reflect the most recent work
- Add ALL new user messages to 'User Messages' section verbatim
- 'Task Specification' can ONLY change if the user explicitly changed direction
- User corrections in 'Errors & Corrections' have highest priority — NEVER remove them
- Do NOT remove information unless a section exceeds its budget
- If a section is over budget, condense OLDEST entries first
- Keep the exact section header format (# Section Name)

{section_budget_warnings}

Current session memory:
{existing_session_memory OR empty template}

New messages since last update (turn {from_turn} to {to_turn}):
{recent_messages, truncated to ~3000 tokens}

Output the complete updated session memory in the same [session-memory:v1] format.
```

**Input budget**: ~3000 tokens for recent messages + ~2000 tokens for existing memory = ~5000 tokens input
**Output budget**: ~2000 tokens
**Cost per update**: ~$0.001 with cheap models

### 4.5 Format Validation

Every L1 update result is validated before storage:

1. Must start with `[session-memory:v1]`
2. Must contain `# Task Specification` section with non-empty content
3. Must contain `# Current State` section with non-empty content
4. Must contain `# User Messages` section

If validation fails:
- The malformed output is discarded (not stored in Memoria)
- The previous valid L1 remains in use
- A warning is logged: `session_memory.validation_failed`
- After 2 consecutive validation failures, the cheap model is considered unreliable for this session; L1 updates are paused and L2 becomes the primary compaction source

### 4.6 Cache-Aware Injection

**Critical design decision**: L1 is injected as a **system message at position 1** (after the main system prompt), NOT into the conversation messages.

```
Message array structure:
  [0] System prompt (Global + Session + Dynamic sections)  ← CACHED PREFIX
      └─ Dynamic section includes L0 anchor
  [1] [Session Memory] system message (L1 injection)       ← NOT in cached prefix
  [2..N-1] Conversation messages                           ← Volatile
  [N] Last message with cache_control breakpoint           ← Cache boundary
```

**Why position 1 as system message**:
- Does NOT break the system prompt's cached prefix (Global + Session sections are in message [0])
- System messages at position 1 are outside the Anthropic cache breakpoint (which is on the last message)
- For OpenAI automatic prefix caching: the primary system message [0] stays identical, so the prefix cache hits
- L1 content changes periodically (~every 5K tokens), which is acceptable for a non-cached position

**Provider-specific position**: The layout above shows the Anthropic path. For OpenAI (automatic prefix caching), message [0] is split into stable (Global+Session) and dynamic (None+L0) system messages, so L1 is at position 2. The principle is the same: L1 is always placed AFTER the stable cached prefix, never inside it.

**Pressure-adaptive injection**:

| Context Pressure | What's Injected | Token Cost |
|-----------------|-----------------|------------|
| < 75% | L1 injection version (compressed) | ~2000 |
| 75–85% | L1 minimal (Task + Current State + Progress only) | ~800 |
| 85–95% | L0 anchor only (in system prompt) | ~50 |
| > 95% | L0 anchor only | ~50 |
| Post-compaction first turn | L1 injection version (full) + continuation prompt | ~2200 |

### 4.7 Memoria Storage Protocol

```
Store/Update L1:
  POST /v1/memories
  {
    "content": "[session-memory:v1]\n# Session Title\n...",
    "memory_type": "working",
    "session_id": "{session_id}"
  }

  On subsequent updates:
  PUT /v1/memories/{memory_id}/correct
  {
    "new_content": "[session-memory:v1]\n# Session Title\n...",
    "reason": "session memory update turn {N}"
  }

Retrieve L1 (at compaction time):
  POST /v1/memories/search
  {
    "query": "[session-memory:v1] session state",
    "top_k": 3,
    "memory_types": ["working"]
  }
  Then filter results client-side:
    - Match session_id
    - Match content prefix "[session-memory:v1]"
    - Take the most recent (by updated_at)
```

**Why `search` + client filter instead of `retrieve`**: The `[session-memory:v1]` prefix is a structural marker, not a semantic concept. Memoria's fulltext index (ngram parser) reliably matches this literal prefix. Using `search` with `memory_types` filter narrows the candidate set. Client-side `session_id` + prefix filtering ensures exact match. The `retrieve` endpoint's session_id filtering is post-retrieval anyway, so there's no efficiency difference.

**Memory ID tracking**: The L1 writer keeps the `memory_id` of the current session memory in-memory. Subsequent updates use `PUT /v1/memories/{id}/correct` directly, avoiding the search round-trip. The search path is only needed when recovering (e.g., session resume, compaction without cached ID).

## 5. L2: Compaction Summary (Fallback)

Used only when L1 is unavailable (first compaction before L1 is populated, or Memoria unreachable).

### 5.1 Nine-Section Prompt

Borrowed from Claude Code's `compact_prompt.ts` with adaptations:

```
CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.

First, write an <analysis> block analyzing the conversation chronologically.
Then, write a <summary> block with these REQUIRED sections:

1. Primary Request and Intent — ALL of the user's explicit requests
2. Key Technical Concepts — technologies, frameworks discussed
3. Files and Code Modified — files examined/modified with code snippets
4. Errors and Fixes — all errors + how fixed + user feedback
5. Problem Solving — problems solved, ongoing troubleshooting
6. All User Messages — list ALL user messages that are not tool results
7. Pending Tasks — tasks explicitly asked to work on
8. Current Work — what was being worked on immediately before this summary,
   with DIRECT QUOTES from the most recent conversation
9. Next Step — DIRECTLY in line with user's most recent explicit requests.
   Do not start on tangential or old completed requests.

The <analysis> block will be stripped. Only <summary> enters context.
```

### 5.2 Anti-Drift Safeguards

- Section 6 preserves all user messages verbatim
- Section 8 requires "direct quotes from the most recent conversation showing exactly what task was being worked on"
- Section 9 requires alignment with "user's most recent explicit requests"
- `format_structured_summary()` warns if required sections (Primary Request, Pending Tasks, Current Work) are missing

### 5.3 Post-Compaction Continuation Prompt

Injected as a user message after the summary:

```
This session is being continued from a previous conversation that was compacted.
The session state above preserves your task and progress.
Continue the conversation from where it left off without asking the user any further questions.
Resume directly — do not acknowledge the summary, do not recap what was happening,
do not preface with "I'll continue" or similar.
Pick up the last task as if the break never happened.
```

### 5.4 Post-Compaction Restoration

Borrowed from Claude Code's `postCompactCleanup.ts`:

| Restore Item | Condition | Budget |
|-------------|-----------|--------|
| Recently read files | Not already in preserved messages (dedup) | Top 5, ≤5K tokens each, ≤25K total |
| Active plan | If plan exists | Full content |
| Invoked skills | If skills were activated | ≤5K per skill, ≤15K total |
| Tool schemas | If tools were discovered mid-session | Delta re-announcement |

**File dedup** (Claude Code pattern): Scan preserved tail messages for `read_file` tool results. Extract file paths. Skip any file that's already visible in the preserved messages during restoration.

## 6. L3: Durable Memory (Memoria)

Not injected into context. Retrieved on demand via Memoria's semantic search.

### 6.1 Memory Types

| Type | memory_type | Content Convention | Lifecycle |
|------|------------|-------------------|-----------|
| Session Memory | `working` | `[session-memory:v1]` prefix | Purged at session end |
| Compaction Archive | `semantic` | `[compaction:{session_id}]` prefix | Permanent |
| Session Episodic | `episodic` | topic/action/outcome structure | Permanent |
| Goal/Plan | `procedural` | `🎯 GOAL:` / `📋 PLAN:` prefix | Cross-session |
| User Profile | `profile` | Free-form preferences | Permanent |
| Knowledge | `semantic` | Reusable learnings from sessions | Permanent |

### 6.2 Session End Governance

```
Session end:
  1. Extract reusable knowledge from L1's Errors & Decisions sections
     → Store as semantic memory (cross-session)
  2. Generate episodic summary (cheap model)
     → Store as episodic memory
  3. Purge working memory for this session_id
     → POST /v1/memories/purge { "topic": "session:{session_id}", "reason": "session ended" }
       (Memoria purge uses topic-based keyword matching, not a session_id field)
  4. Update active Goal status if applicable
     → PUT /v1/memories/{goal_id}/correct
```

### 6.3 Cross-Session Bootstrap

At session start, the per-turn prefetch retrieves relevant memories from all types. Active goals (`🎯 GOAL: ... Status: ACTIVE`) and recent episodic summaries provide cross-session continuity.

## 7. Prompt Cache Strategy

### 7.1 Cache-Friendly Message Layout

```
┌─────────────────────────────────────────────────────────────┐
│ Message [0]: System Prompt                                   │
│   ├─ CacheScope::Global  ─── identity, rules, coding ──┐   │
│   ├─ CacheScope::Session ─── tools, task strategy ──────┤   │
│   │                                          CACHED PREFIX   │
│   ├─ CacheScope::None ───── profile, skills, L0 anchor ─┘   │
│   │                                          NOT CACHED      │
├─────────────────────────────────────────────────────────────┤
│ Message [1]: L1 Session Memory (system role)                 │
│   Content: [session-memory:v1] injection version             │
│   NOT CACHED (changes periodically)                          │
├─────────────────────────────────────────────────────────────┤
│ Messages [2..N-1]: Conversation                              │
│   User messages, assistant responses, tool calls/results     │
│   Microcompact clears old tool results here                  │
│   INCREMENTALLY CACHED (turn-to-turn KV reuse)               │
├─────────────────────────────────────────────────────────────┤
│ Message [N]: Last message                                    │
│   cache_control: { type: "ephemeral" }  ← CACHE BREAKPOINT  │
└─────────────────────────────────────────────────────────────┘
```

### 7.2 Cache Impact Analysis

| Operation | Cache Impact | Mitigation |
|-----------|-------------|------------|
| L0 anchor update | None — in CacheScope::None | Already non-cached |
| L1 injection update | **Moderate** — message [1] changes invalidate conversation KV cache from [1] onward | Acceptable: L1 updates ~every 3-5 turns; conversation messages change every turn anyway, so the incremental cache loss is small relative to the new content being added. Net effect: one turn of extra cache-creation cost per L1 update. |
| L1 periodic extraction | None — runs as separate cheap-model API call | Does not touch main conversation |
| Microcompact | None — modifies volatile messages after cache breakpoint | Content changes are in non-cached region |
| Compaction | Full cache reset (expected) | Notify CacheBreakDetector to suppress false-positive alerts |
| Post-compact restoration | New messages appended after summary | New cache prefix established on next turn |

### 7.3 Cache Break Prevention Rules

1. **L0 anchor goes in `CacheScope::None`** — never in Global or Session sections
2. **L1 injection is a separate system message [1]** — does not modify message [0]'s cached content
3. **L1 extraction uses a separate API call** (cheap model) — does not affect main conversation's cache
4. **Microcompact only touches messages after the cache breakpoint** — preserves cached prefix
5. **Tool schemas remain alphabetically sorted** — prevents score fluctuation from breaking cache
6. **Session-latched cache config** — `PromptCacheConfig` captured once at session start
7. **Compaction notifies `CacheBreakDetector`** — suppresses false-positive alerts after expected cache reset

### 7.4 Anthropic vs OpenAI Cache Handling

**Anthropic** (explicit `cache_control`):
- Message [0] has `cache_control` on last Global section and last Session section
- Message [N] has `cache_control: { type: "ephemeral" }` for turn-to-turn KV reuse
- L1 at message [1] has no `cache_control` — it's between the system prompt cache and the conversation cache

**OpenAI** (automatic prefix caching):
- Message [0] is split into two system messages: stable (Global+Session) and dynamic (None+L0)
- L1 is a third system message
- The stable message stays identical across turns → automatic prefix cache hits
- L1 changes don't affect the stable message's cache

## 8. Deployment Modes

### 8.1 CLI Edge Mode

```
L0: In-memory (process lifetime)
L1: Local file + async sync to Memoria
    - Path: {cwd}/.astra/sessions/{sid}/session-memory.md
      (consistent with existing session_memory_extract.rs write_session_memory_file)
    - Write locally first (low latency, atomic write via .tmp + rename)
    - Background task syncs to Memoria every update
    - Compaction reads local file first, falls back to Memoria
    - Claude Code compatibility: also reads ~/.claude/projects/{cwd}/{sid}/session-memory/summary.md
      via existing SessionMemoryFileCombine::Merge mode
L2: Generated on-demand, stored in Memoria
L3: Memoria (server-side)
```

### 8.2 Web Agent Mode

```
L0: In-memory (request-scoped, reconstructed from L1 on session resume)
L1: Directly in Memoria (no local file)
    - Store/correct via HTTP API
    - Retrieve via HTTP API at compaction time
L2: Generated on-demand, stored in Memoria
L3: Memoria (server-side)
```

### 8.3 Claude Code Compatibility

Existing `SessionMemoryFileCombine` modes in `memoria_compact.rs`:

| Mode | Behavior |
|------|----------|
| `None` | Ignore Claude Code's `summary.md` files |
| `Fallback` | Use CC's file only if Memoria returns nothing |
| `Merge` | Combine both: CC file gets 28% of memory token budget, Memoria gets the rest |

When running alongside Claude Code:
- Claude Code maintains its own `~/.claude/projects/{cwd}/{sid}/session-memory/summary.md`
- astra-engine reads this file during compaction (Merge mode)
- astra-engine also maintains its own L1 in Memoria
- Both sources are combined for maximum context preservation

## 9. Compaction Flow (Revised)

```
Compaction triggered (pressure ≥ 75%):

  Step 1: Preserve anchors
    - Keep all system messages (including L0 anchor in system prompt)
    - Keep first user message (original task) — NEVER remove
    - Keep tail N turn pairs
    - Identify and skip old compaction boundary messages (compact_metadata markers
      from previous compactions) — prevents summary-of-summary nesting

  Step 2: Wait for in-flight L1 update (max 15 seconds)
    - If L1 extraction is in progress, wait for completion
    - If timeout, proceed without L1

  Step 3: Retrieve L1 from Memoria (or local file in CLI mode)
    POST /v1/memories/search { query: "[session-memory:v1]", memory_types: ["working"] }
    Filter by session_id + prefix match

  Step 4: Choose summary source
    ├─ L1 available, valid, and non-empty → Use L1 as summary (ZERO LLM compaction)
    └─ L1 unavailable or malformed → Generate L2 with cheap model (9-section prompt)

  Step 5: Build post-compact message array (with explicit roles)
    [0] role:system  — System prompt (with L0 anchor in dynamic section)
    [1] role:system  — Summary (L1 injection version or L2), with compact_metadata marker
    [2] role:user    — Continuation prompt ("pick up where you left off")
    [3..K] role:user + role:assistant — File restorations (synthetic read_file tool
           call/result pairs for deduped recently-read files)
    [K+1..] — Preserved tail messages (original roles)

  Step 6: Post-compact actions
    - Store compaction summary as semantic memory in Memoria
      with [compaction:{sid}] tag and compact_metadata in content
    - Notify CacheBreakDetector (suppress false-positive alerts)
    - Reset microcompact state
    - Clear section caches (system prompt, tool schemas)

  Step 7: Store updated working memory
    - If L1 was used as summary, no additional store needed
    - If L2 was generated, store it as semantic memory with [compaction:{sid}] tag
```

### 9.1 Multi-Compaction Handling

A long session may compact multiple times. Key invariants:

1. **Old compaction boundaries are identified by `compact_metadata`** in the message. When building the preserved set, these boundary messages are recognized and treated as summaries, not as regular conversation.

2. **L1 is always the latest version** — it's updated incrementally, so the second compaction's L1 already incorporates everything from the first compaction's preserved context. No summary-of-summary problem.

3. **First user message is always preserved** — even across multiple compactions, the original task message is pinned. It may appear immediately after the latest summary message.

4. **Compaction archives accumulate in L3** — each compaction stores a semantic memory with `[compaction:{sid}]` tag. These form a chain of historical snapshots, retrievable for debugging but never re-injected.

## 10. Implementation Plan

| Phase | Scope | Key Changes | Files |
|-------|-------|-------------|-------|
| **P0** | Goal preservation | TieredCompaction/ReactiveCompact preserve first user message; L0 anchor extraction + injection in system prompt; continuation prompt after compaction | `context_compression.rs`, `prompts/system.rs`, `memoria_compact.rs` |
| **P1** | Microcompact improvements | Adaptive retention by pressure; duplicate read elimination; cache-aware clearing | `microcompact.rs` |
| **P2** | L1 Session Memory | Template definition; cheap model extraction; per-section budget governance; Memoria store/correct; injection version compression | New: `session_memory.rs`; Modified: `bridge_inprocess.rs`, `server_loop_host.rs` |
| **P3** | Zero-LLM compaction | L1 replaces L2 at compaction time; post-compact file restoration with dedup | `memoria_compact.rs`, `compact_prompt.rs` |
| **P4** | L2 improvements | 9-section prompt with analysis scratchpad; anti-drift safeguards; format validation | `compact_prompt.rs` |
| **P5** | L3 governance | Session end: knowledge backflow, episodic summary, working memory purge, goal update | `agentic_loop_lifecycle.rs`, `memoria_compact.rs` |
| **P6** | Cloud-edge sync | CLI local file + async Memoria sync; web agent direct Memoria; CC compatibility | `session_memory.rs`, `memoria_compact.rs` |

## 11. Metrics & Observability

| Metric | Source | Purpose |
|--------|--------|---------|
| `session_memory.update_count` | L1 update trigger | Track extraction frequency |
| `session_memory.update_latency_ms` | Cheap model API call | Monitor extraction cost |
| `session_memory.injection_tokens` | Token count of injected L1 | Track token overhead |
| `compaction.summary_source` | `l1_session_memory` / `l2_llm_summary` / `l2_fallback` | Track zero-LLM compaction rate |
| `compaction.goal_preserved` | Boolean: was first user message + L0 anchor present post-compact | Verify goal preservation |
| `cache.hit_rate` | Existing cache diagnostics | Verify memory injection doesn't degrade cache |
| `cache.break_reason` | Existing cache break detector | Detect if L1 injection causes unexpected breaks |

## 12. Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| Cheap model produces low-quality L1 | Goal drift, incorrect state | Format validation (4.5): must contain required sections; 2 consecutive failures → pause L1, use L2 |
| L1 update latency blocks main turn | User-visible delay | Async fire-and-forget (4.4); result available next turn; compaction waits max 15s then falls back to L2 |
| Memoria unavailable during compaction | No L1 available | Fallback chain: local file (CLI) → L2 (LLM summary) → pure truncation with first-user-message preserved |
| L1 injection invalidates conversation KV cache | Higher cache-creation costs | L1 updates ~every 3-5 turns; conversation changes every turn anyway; net cost is one turn of extra cache-creation per update (7.2) |
| Per-section budgets too restrictive | Important info truncated | Stored version has generous budgets (4K total); injection version is separately compressed (2K); condensation prompt prioritizes Task/CurrentState/UserMessages |
| Claude Code compatibility conflicts | Duplicate/conflicting memories | Merge mode with budget split (28% CC / 72% Memoria); CC file is read-only, never modified by astra-engine |
| Multiple compactions nest summaries | Summary-of-summary quality degradation | Old compaction boundaries identified by compact_metadata and skipped (9.1); L1 is always latest version, not derived from previous summary |
| Session resume without cached memory_id | Cannot update L1 via correct endpoint | Search fallback: `POST /v1/memories/search` with prefix filter recovers the memory_id (4.7) |

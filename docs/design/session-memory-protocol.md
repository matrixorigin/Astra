# Session Memory Protocol — Design Document

**Status**: Draft (revised — hybrid state model)
**Author**: astra-engine team
**Date**: 2026-04-16
**References**: Claude Code compaction system analysis, Memoria API

## 1. Problem Statement

Two problems:

**P1 — Goal loss**: Context compaction drops the user's original task. Observed in session `f3ef23fe`: after proactive compression at 83% pressure, the agent said "The original user request was lost during context compaction (35 earlier messages removed)." Root cause: `TieredCompaction` preserves system messages + tail N turns, but the first user message falls in the "middle" and gets deleted.

**P2 — L1 is narrative, not truth**: The entire L1 session memory is LLM-generated. `Current State`, `Progress`, `Key Files` are all "summaries" that can hallucinate. When injected at compaction, the model resumes from a potentially wrong state. We're using "summaries" where we need a "state machine."

### Goals

1. **Goal preservation**: The user's original task must never be lost, under any compaction pressure
2. **Ground truth first**: System-tracked facts (files, tools, errors, plan) take priority over LLM narrative
3. **Token efficiency**: Memory injection must be cache-friendly and pressure-adaptive, not a fixed tax
4. **Continuous high-quality memory**: Structured, governed session state that survives multiple compactions
5. **Deployment flexibility**: Works in CLI (edge), web agent (server), and Claude Code compatibility modes
6. **Prompt cache preservation**: Memory operations must not break the cached prefix

## 2. Architecture Overview

### 2.1 Hybrid Memory Pyramid

```
         ┌──────────────────────┐
    L0   │  Anchor + MC          │  Cost: zero LLM, every turn
         │  ~60t + savings       │  Anchor from system facts, microcompact clears old tool results
         ├──────────────────────┤
   L1a   │  Session Facts        │  Cost: zero LLM, every turn
         │  ~150t inject         │  System-tracked: files, tools, errors, plan (ground truth)
         ├──────────────────────┤
   L1b   │  Session Narrative    │  Cost: cheap model, periodic
         │  ≤500t inject         │  LLM-generated: task spec, decisions, user corrections, learnings
         │  ≤2000t stored        │
         ├──────────────────────┤
    L2   │  Compact Summary      │  Cost: cheap model, only when L1a also unavailable (extreme fallback)
         │  ≤3000t               │  9-section LLM summary with anti-drift safeguards
         ├──────────────────────┤
    L3   │  Durable Memory       │  Cost: zero/low, session end
         │  not injected         │  Episodic, procedural, semantic — retrieved on demand via Memoria
         └──────────────────────┘
```

Key change from v1: L1 is split into **L1a (system facts, ground truth)** and **L1b (LLM narrative, supplement)**. L1a is always available (updated every turn, zero LLM). L1b is periodic and optional. Compaction no longer needs to wait for L1b — L1a alone is sufficient for zero-LLM compaction. See Appendix A for v1 architecture.

### 2.2 Design Principles

| Principle | Practice | Implementation |
|-----------|----------|----------------|
| Facts over narrative | System-tracked state takes priority over LLM summaries | L1a facts injected before L1b narrative; contradicted narrative sections skipped (not just warned) |
| Anti-drift | User messages preserved + user corrections never deleted | L1b "User Corrections" section + first user message pin |
| Cache-aware | Never inject into stable prefix | Memory in CacheScope::None; injection after cache breakpoint |
| Layered compaction | Microcompact → session facts → narrative → LLM summary | L0 MC → L1a facts → L1b narrative → L2 LLM summary |
| Size governance | Per-section budgets, total injection ≤700t | L1a ~150t + L1b ~500t |
| Zero-LLM compaction | Session facts always available, no waiting | L1a replaces L2 at compaction; L1b enriches if available |
| LLM does LLM things | Don't ask LLM to track files/errors/progress | L1b only: task spec, decisions, corrections, learnings |
| Post-compact restoration | Re-inject top 5 files, plans, skills, tool schemas | File dedup against preserved messages |

## 3. L0: Anchor + Microcompact

### 3.1 Session Anchor

A compact task summary embedded in the system prompt's dynamic section (`CacheScope::None`). Never deleted by any compaction layer. **Generated from system facts, not LLM narrative.**

**Format**:
```
[session-anchor] Goal: {task}. State: {system_state}. {constraints}.
```

**Examples**:
```
[session-anchor] Goal: Add OAuth support to API with JWT tokens. State: 3/5 subtasks, current: token refresh. Last error: sqlx migration column exists.
[session-anchor] Goal: Fix auth module login bug. State: write src/auth.rs (t7). Avoid: rm, git push --force.
```

**Generation** (from SessionFacts, zero LLM):

```rust
fn extract_anchor(first_user_msg: &str, facts: &SessionFacts, narrative: Option<&SessionMemory>) -> String {
    let task = narrative
        .and_then(|n| n.section("Task Specification"))
        .map(|s| first_sentence(s))
        .unwrap_or_else(|| truncate_words(first_user_msg, 20));
    let state = if let Some(plan) = &facts.plan_state {
        format!("{}/{} subtasks, current: {}", plan.completed, plan.total,
            plan.current_subtask.as_deref().unwrap_or("unknown"))
    } else if let Some(f) = facts.active_files.last() {
        format!("{} {} (t{})", f.last_action, f.path, f.turn)
    } else { "starting".to_string() };
    let mut anchor = format!("[session-anchor] Goal: {task}. State: {state}.");
    if let Some(err) = &facts.error_state.last_error {
        anchor.push_str(&format!(" Last error: {}.", truncate_words(err, 10)));
    }
    if !facts.blocked_tools.is_empty() {
        anchor.push_str(&format!(" Avoid: {}.", facts.blocked_tools.join(", ")));
    }
    anchor
}
```

> **Migration**: v1 anchor derives from L1 narrative sections (`Current State`, `Progress`). This version derives from `SessionFacts` — always available, ground truth.

**Lifecycle**:
- **Created**: Turn 1, from first user message + empty facts
- **Updated**: Every turn, re-derived from `SessionFacts` + narrative Task Specification
- **Injected**: In `CacheScope::None` dynamic section
- **Compaction**: Never removed
- **Cost**: ~60 tokens (up from v1's ~50t, but much higher information density)

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

**State-aware compaction** (uses `SessionFacts.active_files` as pin list):

```
For each tool_result in compactable messages:
  file = extract_file_path(tool_result)
  if file ∈ active_files (last 5 turns) → PIN: never compact, keep full content
  if file ∈ active_files (older turns)  → keep metadata only (path + size), compact content
  if file ∉ active_files                → aggressive drop (replace with placeholder)
```

This shifts compaction from pure token-counting to **semantic-aware GC**: files the agent is actively working on are preserved regardless of token pressure, while irrelevant file reads are dropped first. The pin list is free — it comes from L1a facts which are updated every turn at zero cost.

**Cache impact**: Microcompact modifies message content in the volatile region (after the cache breakpoint). It does NOT affect the cached prefix. After microcompact, the cache breakpoint is re-placed on the last message.

## 4. L1: Session State

L1 is split into **L1a (system facts, ground truth)** and **L1b (LLM narrative, supplement)**. L1a is always available; L1b enriches when available but is never blocking.

> **Current state**: v1 uses a monolithic 10-section L1 generated by `build_l1_from_messages()`. See Appendix A for v1 details and known limitations.

### 4.1 L1a: Session Facts (Ground Truth)

System-tracked state, updated every turn from journal events and tool call records. Zero LLM cost. Always available.

```rust
/// Ground truth session state. Never hallucinated.
pub struct SessionFacts {
    pub active_files: Vec<FileEntry>,       // path + last_action + turn, last 20
    pub recent_tool_calls: Vec<ToolFact>,   // name + ok/fail + turn, last 10
    pub plan_state: Option<PlanFact>,       // goal + completed/total + current_subtask
    pub blocked_tools: Vec<String>,
    pub error_state: ErrorFact,             // count + last_error_msg + turn
    pub turn: u32,
    pub estimated_tokens: u64,
}
```

**Data sources**:

| Field | Source | How |
|-------|--------|-----|
| `active_files` | `ToolCallRecord` at tool execution time | Tool executor extracts `path` from full arguments *before* truncation to `args_preview`. Stored as `ToolCallRecord.file_path: Option<String>` (new field, ~30 bytes). NOT parsed from `args_preview` (which is truncated to 80 chars and unreliable for `str_replace` where path may be buried). |
| `recent_tool_calls` | `JournalEvent.tool_calls` | Direct: `name`, `ok` fields already exist |
| `plan_state` | `HeavyCheckpoint` (not journal) | `checkpoint.executing_plan_json` contains the full plan with step statuses. Parsed at session resume and updated when plan steps complete. Journal only has `plan_subtask_id` (not enough). |
| `blocked_tools` | `HeavyCheckpoint.blocked_tools` | Direct field access |
| `error_state` | `JournalEvent` where `error.is_some()` | Incremental: count errors, keep last error message |
| `turn` / `estimated_tokens` | `JournalEvent.turn` / `tokens_in` | Direct field access |

**⚠️ Implementation note**: `ToolCallRecord` currently has `args_preview: Option<String>` (truncated ~80 chars) but no `file_path` field. P1 must add `file_path: Option<String>` to `ToolCallRecord`, populated by the tool executor from the full arguments before truncation. This is a ~5-line change in the tool execution path.

**Injection format** (~150 tokens):
```
# System State
Turn 12, ~45K tokens
Plan: Implement OAuth (3/5 subtasks), current: token refresh
Active files:
  write src/auth/refresh.rs (t11)
  read src/auth/mod.rs (t10)
Errors: 2 total, last: sqlx migration column exists (t9)
Blocked tools: web_fetch
```

**Cost**: ~150 tokens, updated every turn, zero LLM.

### 4.2 L1b: Session Narrative (LLM-Generated)

Only covers what LLM is good at: task understanding, decisions, user corrections, learnings. Does NOT duplicate system-tracked state.

**Template (6 sections, stored ≤2000t)**:

```markdown
[session-memory:v1]
# Session Title
{5-10 word descriptive title}

# Task Specification
{What the user originally asked. IMMUTABLE unless user explicitly changes direction.}

# User Corrections
{User corrections and explicit preferences. NEVER remove. Highest priority.}

# Learnings
{Patterns, gotchas, conventions discovered. Reusable across sessions.}

# Decisions
{Key technical decisions and rationale. Last 5.}

# User Messages
{Last 5 user messages verbatim. Older → 1-line summary: "Turn N: {what}"}
```

**Removed from v1**: Current State, Key Files, Progress, Worklog, Context — all replaced by L1a system facts.

**Size governance (stored version, ≤2000t)**:

| Section | Max Tokens | Condensation Rule |
|---------|-----------|-------------------|
| Session Title | 20 | Never condense |
| Task Specification | 200 | 🔴 IMMUTABLE unless user changes direction |
| User Corrections | 300 | 🔴 NEVER remove |
| Learnings | 300 | 🟡 Drop oldest if over budget |
| Decisions | 400 | 🟡 Keep last 5, drop oldest |
| User Messages | 800 | Last 5 verbatim; older → 1-line summary; drop oldest summaries first |
| **Total** | **≤2000** | Half of v1's 4000t budget |

**User Messages sliding window detail**: The "older → 1-line summary" is done by the LLM during extraction. The extraction prompt receives `recent_messages` (truncated to ~3000t). If more than 5 new user messages arrived since last extraction, the LLM sees all of them in the input and decides which 5 are "last 5" (verbatim) vs older (summarized). If the input window misses some messages (>3000t of messages since last extraction), those messages are lost — this is acceptable because extraction triggers every ~5K tokens, so the 3000t input window covers the gap with margin. For sessions with very long user messages, the extraction trigger threshold should be lowered.

**v1 vs v2.1 section comparison**:

| Section | v1 | v2.1 | Rationale |
|---------|----|------|-----------|
| Session Title | ✅ | ✅ | Same |
| Task Specification | ✅ | ✅ | Same |
| Current State | ✅ (LLM-derived) | ❌ → L1a | Replaced by system facts |
| Key Files | ✅ (from tool args) | ❌ → L1a | Replaced by SessionFacts.active_files |
| Progress | ✅ (emoji count) | ❌ → L1a | Replaced by SessionFacts.plan_state |
| Errors & Corrections | ✅ (mixed) | ❌ → L1a + User Corrections | Errors → L1a; Corrections → dedicated section |
| Decisions | ✅ | ✅ | Same |
| User Messages | ✅ | ✅ | Same |
| Worklog | ✅ | ❌ | Redundant with journal |
| Context | ✅ | ❌ | Redundant with anchor |

**Injection version** (≤500t, rule-compressed, zero LLM):

| Section | Injection Rule |
|---------|---------------|
| Task Specification | Full text |
| User Corrections | Full text (never drop) |
| Learnings | Last 3 entries |
| Decisions | Last 2 entries, ≤15 words each |
| User Messages | Last 3 verbatim only |
| Session Title | Omitted |

**⚠️ Migration note**: v1 `compress_to_injection` still uses all 10 sections. The injection budget reduction (2000t → 500t) requires the L1a/L1b split first.

### 4.4 Combined Injection: Facts-First Assembly

Assembles L1a facts + L1b narrative with cross-validation.

> **Migration**: v1 `compress_to_injection()` compresses the 10-section L1 by filtering sections. This is replaced by `build_injection()` after P1–P4.

```rust
pub fn build_injection(facts: &SessionFacts, narrative: Option<&SessionMemory>) -> String {
    let mut out = String::from("[session-memory]\n");

    // ── Layer 1: System Facts (ground truth, ~150t) ──
    out.push_str(&facts.to_injection());

    // ── Layer 2: LLM Narrative (supplement, ≤500t) ──
    if let Some(n) = narrative {
        if let Some(task) = n.section("Task Specification") {
            out.push_str(&format!("# Task\n{}\n", truncate_to_token_budget(task, 200)));
        }
        if let Some(corrections) = n.section("User Corrections") {
            if !corrections.trim().is_empty() {
                out.push_str(&format!("# User Corrections\n{}\n",
                    truncate_to_token_budget(corrections, 150)));
            }
        }
        if let Some(learnings) = n.section("Learnings") {
            let entries: Vec<&str> = learnings.lines()
                .filter(|l| l.trim().starts_with("- ")).collect();
            let last_three: Vec<&str> = entries.iter().rev().take(3).rev().copied().collect();
            if !last_three.is_empty() {
                out.push_str("# Learnings\n");
                for line in last_three { out.push_str(line.trim()); out.push('\n'); }
            }
        }
        if let Some(decisions) = n.section("Decisions") {
            let entries: Vec<&str> = decisions.lines()
                .filter(|l| l.trim().starts_with("- ")).collect();
            if let Some(recent) = entries.last() {
                out.push_str(&format!("# Last Decision\n{}\n", recent.trim()));
            }
        }
    }

    // ── Layer 3: Cross-validation (detect AND act) ──
    // When facts contradict narrative, don't just warn — downgrade the contradicted section.
    // Rationale: a warning string may be ignored by the model. Omitting the bad section is reliable.
    let mut stale_sections: Vec<&str> = Vec::new();

    if facts.error_state.total_errors > 0 && facts.error_state.last_error.is_some() {
        let narrative_implies_done = facts.plan_state.as_ref()
            .map(|p| p.completed == p.total && p.total > 0)
            .unwrap_or(false);
        if narrative_implies_done {
            // Plan says done but errors exist → don't trust narrative's Task Spec
            // (it may have been updated to say "completed")
            stale_sections.push("Task Specification");
            out.push_str("⚠️ Plan complete but unresolved errors — narrative Task section omitted, using facts only\n");
        }
    }
    if let Some(n) = narrative {
        if let Some(corrections) = n.section("User Corrections") {
            if corrections.trim().is_empty() && facts.error_state.total_errors >= 3 {
                // Many errors but no corrections recorded → mark for priority re-extraction
                stale_sections.push("User Corrections");
                out.push_str("⚠️ 3+ errors but no corrections recorded — triggering re-extraction\n");
            }
        }
    }

    // Stale sections are skipped in the narrative injection above (Layer 2 checks stale_sections).
    // Also mark them for priority update on next L1b extraction cycle.

    out
}
```

**Cross-validation actions** (not just warnings):

| Contradiction | Detection | Action |
|--------------|-----------|--------|
| Plan complete + unresolved errors | `plan.completed == plan.total && error_state.total_errors > 0` | **Skip** narrative Task Specification from injection; facts alone provide state |
| Many errors + no corrections | `error_state.total_errors >= 3 && corrections.is_empty()` | **Trigger** immediate L1b re-extraction to capture missed corrections |
| Narrative file list ≠ facts file list | Future: diff `active_files` vs narrative Key Files | N/A (Key Files removed from L1b, so this contradiction is eliminated by design) |

The key insight: **omitting a contradicted section is more reliable than warning the model**, because the model may ignore warnings but cannot hallucinate from data that isn't there.

### 4.5 Update Trigger

L1a (facts): **Every turn**, zero cost.

L1b (narrative): Dual-threshold + error-triggered:

```
Initial gate: context tokens ≥ 10,000

Subsequent updates require BOTH:
  - Token growth ≥ 5,000 since last extraction
  - Tool calls ≥ 3 since last extraction

OR: Token growth ≥ 5,000 AND last assistant turn has no tool calls (natural break)

Error trigger:
  - TurnError occurred this turn AND context tokens ≥ 10,000
  (captures user corrections immediately, before compaction can drop them)
```

> **Migration**: v1 only has the dual-threshold. Error-triggered extraction is added in P3.

### 4.6 Update Mechanism (L1b Only)

Uses a **cheap text model**. Async fire-and-forget — main turn does NOT block.

> **Migration**: v1 uses `build_l1_from_messages()` (synchronous, scans conversation, no LLM call). The LLM-based extraction module exists in `session_memory_extract.rs` but is not yet wired to the runtime loop.

**Update prompt** (LLM only updates what LLM is good at):

```
You are updating session notes based on new conversation messages.

You ONLY update these sections:
- Task Specification: ONLY change if user explicitly changed direction
- User Corrections: Add any new user corrections or preferences. NEVER remove existing ones.
- Learnings: Add patterns, gotchas, conventions discovered
- Decisions: Add key technical decisions with rationale (keep last 5)
- User Messages: Last 5 verbatim, older → 1-line summary "Turn N: {what}"

Do NOT write Current State, Progress, Key Files, Errors — the system tracks those separately.

Current session notes:
{existing_narrative OR empty template}

New messages since last update (turn {from_turn} to {to_turn}):
{recent_messages, truncated to ~3000 tokens}

Output the complete updated notes in [session-memory:v1] format.
```

### 4.7 Format Validation

Same as v1: must start with `[session-memory:v1]`, must contain `# Task Specification` with non-empty content. After 2 consecutive validation failures, L1b updates are paused — L1a (facts) alone is sufficient.

### 4.8 Pressure-Adaptive Injection

| Context Pressure | What's Injected | Token Cost |
|-----------------|-----------------|------------|
| < 75% | L1a facts + L1b narrative | ~650 |
| 75–85% | L1a facts only | ~150 |
| ≥ 85% | L0 anchor only | ~60 |
| Post-compaction | L1a facts + L1b narrative + continuation prompt | ~850 |

v1 injects up to 2000t at < 75%. This design injects ~650t. **Saves ~1350t/turn.**

### 4.9 Memoria Storage Protocol

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
| L1 injection update | **Low** — message [1] changes; L1a+L1b combined ~650t (was 2000t in v1) | L1a updates every turn but is small (~150t); L1b updates ~every 3-5 turns |
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
L0:  In-memory (process lifetime), derived from L1a facts
L1a: In-memory (process lifetime), persisted in journal events + checkpoint
     Rebuilt on session resume from journal + HeavyCheckpoint
L1b: Local file + async sync to Memoria
     Path: {cwd}/.astra/sessions/{sid}/session-memory.md
     Claude Code compat: reads ~/.claude/projects/{cwd}/{sid}/session-memory/summary.md
L2:  Generated on-demand (extreme fallback only)
L3:  Memoria (server-side)
```

### 8.2 Web Agent Mode

```
L0:  In-memory, derived from L1a facts
L1a: In-memory, persisted via cloud checkpoint
     Rebuilt on session resume from cloud events + checkpoint
L1b: Directly in Memoria (no local file)
L2:  Generated on-demand (extreme fallback only)
L3:  Memoria (server-side)
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

## 9. Compaction Flow

```
Compaction triggered (pressure ≥ 75%):

  Step 1: Preserve anchors
    - Keep all system messages (including L0 anchor in system prompt)
    - Keep first user message (original task) — NEVER remove
    - Keep tail N turn pairs
    - Skip old compaction boundary messages (compact_metadata markers)

  Step 2: Build SessionFacts from journal (zero LLM, instant)
    - Always available — no waiting, no fallback needed
    - This alone is sufficient for zero-LLM compaction

  Step 3: Retrieve L1b narrative from Memoria (or local file in CLI mode)
    - Best-effort: if available, enriches the summary
    - If unavailable (Memoria down, extraction not yet triggered): proceed without it

  Step 4: Choose summary source
    ├─ L1a facts available (always) → Use facts as summary base (ZERO LLM)
    │   ├─ L1b narrative available → Merge: facts + task spec + corrections + learnings
    │   └─ L1b narrative unavailable → Facts alone (still zero LLM, still good)
    └─ L1a facts somehow unavailable (journal corrupted) → Generate L2 with cheap model

  Step 5: Build post-compact message array
    [0] role:system  — System prompt (with L0 anchor)
    [1] role:system  — Summary (L1a facts + L1b narrative), with compact_metadata marker
    [2] role:user    — Continuation prompt
    [3..K] — File restorations (deduped)
    [K+1..] — Preserved tail messages

  Step 6: Post-compact actions
    - Store compaction summary as semantic memory in Memoria
    - Notify CacheBreakDetector
    - Reset microcompact state
```

> **Migration from v1**: v1 compaction retrieves L1 narrative from Memoria and waits up to 15s if extraction is in-flight. This design eliminates the wait — L1a facts are always available locally.

### 9.1 Multi-Compaction Handling

A long session may compact multiple times. Key invariants:

1. **Old compaction boundaries are identified by `compact_metadata`** in the message. When building the preserved set, these boundary messages are recognized and treated as summaries, not as regular conversation.

2. **L1 is always the latest version** — it's updated incrementally, so the second compaction's L1 already incorporates everything from the first compaction's preserved context. No summary-of-summary problem.

3. **First user message is always preserved** — even across multiple compactions, the original task message is pinned. It may appear immediately after the latest summary message. ✅ Implemented in `session_memory_protocol.rs`.

4. **Compaction archives accumulate in L3** — each compaction stores a semantic memory with `[compaction:{sid}]` tag. These form a chain of historical snapshots, retrievable for debugging but never re-injected.

**⚠️ v1 limitation**: Without L1a (facts), the second compaction depends on the L1 narrative generated from the first compaction's preserved messages. Any hallucination in the L1 narrative propagates. v2.1 eliminates this by making L1a (facts) the authoritative source.

## 10. Implementation Plan

> v1 current state is documented in Appendix A.

| Phase | Scope | Key Changes | Files |
|-------|-------|-------------|-------|
| **P1** | L1a Session Facts | `SessionFacts` struct; `ToolCallRecord.file_path` field; update from journal events every turn; `to_injection()` | New: `session_facts.rs`; Modified: tool execution path for `file_path` |
| **P2** | L0 anchor from facts | `extract_anchor()` uses `SessionFacts` for State/Constraints instead of L1 narrative | `session_memory_protocol.rs` |
| **P3** | L1b narrative slimdown | Template from 10→6 sections; extraction prompt only updates LLM-appropriate sections; error-triggered extraction | `session_memory_extract.rs`, `session_memory_protocol.rs` |
| **P4** | Facts-first injection | `build_injection()` assembles L1a + L1b with cross-validation; pressure-adaptive levels | `session_memory_protocol.rs` |
| **P5** | Facts-first compaction | Compaction uses L1a (always available) as base; L1b enriches; remove blocking wait | `memoria_compact.rs` |
| **P6** | Microcompact improvements | Adaptive retention by pressure; duplicate read elimination; `SessionFacts.active_files` as pin list | `microcompact.rs` |
| **P7** | L3 governance | Session end: knowledge backflow from L1b Learnings + User Corrections; working memory purge | `agentic_loop_lifecycle.rs` |

### Known Gaps

| Gap | Severity | Notes |
|-----|----------|-------|
| No `SessionFacts` struct | High | Cornerstone of the design — blocks P1–P5 |
| No `ToolCallRecord.file_path` field | High | P1 prerequisite — `args_preview` is unreliable for file path extraction |
| `plan_state` requires checkpoint access | Medium | Journal only has `plan_subtask_id`; full plan is in `HeavyCheckpoint.executing_plan_json` |
| Error-triggered extraction not wired | Medium | User corrections between extraction cycles may be lost |
| Cross-validation not implemented | Medium | No warning when narrative contradicts system facts |
| L1b extraction via LLM not wired to runtime | Medium | `session_memory_extract.rs` has `build_extraction_prompt()` but it's not called from the agentic loop |
| Post-compact restoration not implemented | Low | File re-injection, plan restoration, skill re-announcement |
| `truncate_to_token_budget` not CJK-aware | Medium | Needs `floor_char_boundary` guard |
| Memoria fallback for L1 persistence | Medium | `persist_l1` gives up after 2 failures with no local file fallback |

## 11. Metrics & Observability

| Metric | Source | Purpose |
|--------|--------|---------|
| `session_facts.update_count` | L1a turn-end update | Track facts freshness |
| `session_facts.active_files_count` | L1a | Monitor file tracking coverage |
| `session_narrative.update_count` | L1b extraction trigger | Track narrative extraction frequency |
| `session_narrative.update_latency_ms` | Cheap model API call | Monitor extraction cost |
| `session_memory.injection_tokens` | Token count of L1a+L1b injection | Track token overhead (target: ≤700t) |
| `session_memory.cross_validation_warnings` | `build_injection()` | Track facts/narrative inconsistencies |
| `compaction.summary_source` | `l1a_facts` / `l1a_plus_narrative` / `l2_llm_summary` | Track zero-LLM compaction rate |
| `compaction.goal_preserved` | Boolean: first user message + L0 anchor present post-compact | Verify goal preservation |
| `cache.hit_rate` | Existing cache diagnostics | Verify injection doesn't degrade cache |
| `session_memory.learnings_backflow_count` | `extract_learnings_for_backflow()` | Track knowledge persistence |

## 12. Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| Cheap model produces low-quality L1b narrative | Incorrect task spec or decisions | Format validation (4.7); 2 consecutive failures → pause L1b; L1a facts alone is sufficient |
| L1b update latency blocks main turn | User-visible delay | Async fire-and-forget; result available next turn; compaction does NOT wait (uses L1a facts) |
| Memoria unavailable during compaction | No L1b narrative | L1a facts always available locally; L2 only needed if journal also corrupted |
| L1a facts miss some state | Incomplete file/tool tracking | Facts are best-effort; narrative supplements; cross-validation catches inconsistencies |
| L1b narrative contradicts L1a facts | Confusing injection | Cross-validation **skips** contradicted narrative sections + triggers re-extraction; model cannot hallucinate from omitted data |
| Per-section budgets too restrictive | Important info truncated | Stored version has 2000t total (generous for 6 sections); injection separately compressed |
| User corrections lost before extraction | Correction dropped by compaction | Error-triggered extraction (4.5): TurnError → immediate L1b update captures correction |
| Multiple compactions nest summaries | Summary-of-summary degradation | L1a facts are always fresh (not derived from previous summary); old boundaries identified by compact_metadata |
| Session resume without cached facts | Cannot restore L1a | `SessionFacts::from_journal_and_checkpoint()` rebuilds from persisted journal + checkpoint |
| `ToolCallRecord.args_preview` truncates file path | `active_files` misses files from `str_replace` calls | New `file_path` field on `ToolCallRecord`, populated at execution time from full args (P1 prerequisite) |
| `build_l1_from_messages` scans ALL messages | O(n) per turn for long sessions | Replaced by incremental `SessionFacts.update_from_turn()` (O(1) per turn) |
| `truncate_to_token_budget` not CJK-aware | May truncate mid-character for Chinese | Needs `floor_char_boundary` guard (same pattern as UTF-8 bugfix in cli_formatting.rs) |


## Appendix A: v1 Current Implementation

The current L1 is a monolithic `SessionMemory` struct with 10 sections, all populated by `build_l1_from_messages()` in `session_memory_protocol.rs`.

```rust
pub const SESSION_MEMORY_PREFIX: &str = "[session-memory:v1]";

const REQUIRED_SECTIONS: &[&str] = &["Task Specification", "Current State", "User Messages"];

const SECTION_NAMES: &[&str] = &[
    "Session Title", "Task Specification", "Current State", "Key Files",
    "Progress", "Errors & Corrections", "Decisions", "User Messages",
    "Worklog", "Context",
];
```

**Budget**: Stored 4000t, injection 2000t via `compress_to_injection()`.

**Generation**: `build_l1_from_messages()` scans all conversation messages to extract task (first user msg), files (tool call `path` args), tools (`tool_calls[].function.name`), current state (last assistant msg, 15 words), progress (tool call count), user messages (deduped, last 10).

**Known limitations**:
1. `Current State`, `Progress`, `Key Files` are LLM-derivative, not system facts
2. No dedicated "User Corrections" section — corrections buried in "Errors & Corrections"
3. No cross-validation between facts and narrative
4. No error-triggered extraction — only dual-threshold (token growth + tool calls)
5. `count_progress_markers` uses emoji counting (✅/🔄/⏳), not real plan steps
6. `truncate_to_token_budget` not CJK-aware (unlike `truncate_words`)

**Implemented components** (line references to `session_memory_protocol.rs`):

| Component | Lines |
|-----------|-------|
| L0 Anchor (from L1) | 13–23 |
| L1 Parse/Validate | 148–238 |
| `build_l1_from_messages` | 470–606 |
| `compress_to_injection` | 241–347 |
| 10-section template + budgets | 112–145 |
| First user message pin | 381–388 |
| Compaction boundary detection | 391–393 |
| Pressure-adaptive injection levels | 397–432 |
| L1 persist to Memoria | 438–464 |
| CJK-aware `truncate_words` | 36–67 |
| Anthropic content block support | 350–366 |
| User message deduplication | 482–491 |

---
name: optimize-prompt
description: "Developer skill: analyze and optimize the LLM prompt — system message, tool schemas, history, skill injections, budget pressure. Identifies token waste and context bloat in astra's prompt assembly pipeline."
user_invocable: true
when_to_use: "When the user wants to analyze or reduce the LLM prompt size, find token waste, or optimize context assembly"
arguments:
  - name: TARGET
    description: "Session ID, debug JSON path, or 'this'/'last'. Omit for most recent."
    required: false
  - name: COMPONENT
    description: "Focus on: 'system', 'tools', 'history', 'skills', 'budget', or 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Optimize Prompt

Analyze and optimize everything astra sends to the LLM — the system prompt, tool
schemas, conversation history, skill injections, and memory signals. Identifies token
waste, context bloat, and sub-optimal budget pressure management.

## Task

$ARGUMENTS

---

## Phase 1: Load Prompt Data

### 1.1 Resolve TARGET

Same as other skills — journal JSONL or debug JSON files.

### 1.2 Best Data Sources for Prompt Analysis

| Source | What It Shows | Path |
|--------|-------------|------|
| Heavy checkpoint | Full message array sent to LLM | `~/.astra/sessions/<id>/step_checkpoints/*-heavy.json` |
| Debug dump (full) | Complete turn snapshot with schema `astra-debug-turn-full-v1` | `/tmp/debug-<id>-turn<N>-full.json` |
| Journal | Per-turn token counts, tools_selected, budget_pressure | `~/.astra/sessions/<id>.jsonl` |

**Heavy checkpoints are the gold standard** — they contain the exact messages sent to the LLM.

```bash
# Find latest heavy checkpoint
ls -lt ~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json | head -3

# Read it (JSON array of OpenAI-format messages)
cat <checkpoint_file> | python3 -c "
import json, sys
msgs = json.load(sys.stdin)
for i, m in enumerate(msgs):
    role = m.get('role', '?')
    content = m.get('content', '')
    tool_calls = m.get('tool_calls', [])
    size = len(json.dumps(m))
    print(f'{i:3d} | {role:10s} | {size:7,d} bytes | tools={len(tool_calls)} | {content[:80]}...')
"
```

---

## Phase 2: System Prompt Analysis

### 2.1 System Prompt Components

Astra's system prompt is built from layered sections (`prompts/system.rs`):

| Component | Cache Scope | Typical Size | Purpose |
|-----------|-------------|-------------|---------|
| Base identity | Global | ~200 tokens | "You are an expert software engineer..." |
| Core rules | Global | ~500 tokens | Think step-by-step, no fabrication, tool usage |
| Tool-conditional guidance | Session | ~2-8K tokens | Guidance per available tool category |
| Task type rules | Session | ~500 tokens | Classification-specific behavior |
| Output style | Session | ~200 tokens | Markdown, code style preferences |
| Project profile (edge_profile) | None | ~500-2K tokens | Git branch, workspace state, project type |
| Active skills | None | ~0-5K tokens | Injected skill instructions |
| Memory signals | None | ~0-1K tokens | Relevant memories from learning system |

**Default total**: ~14,000 tokens (baseline estimate used by `estimate_tokens_precise`)

### 2.2 Measure Actual System Prompt

From heavy checkpoint, extract the system message:

```bash
cat <checkpoint_file> | python3 -c "
import json, sys
msgs = json.load(sys.stdin)
system_msgs = [m for m in msgs if m.get('role') == 'system']
total = sum(len(json.dumps(m)) for m in system_msgs)
print(f'System messages: {len(system_msgs)}')
print(f'Total bytes: {total:,}')
print(f'Estimated tokens: {total // 4:,}')
for i, m in enumerate(system_msgs):
    content = m.get('content', '')
    print(f'  [{i}] {len(content):,} chars: {content[:100]}...')
"
```

Flag:
- 🔴 System prompt >20K tokens (excessive — model will deprioritize later instructions)
- 🟡 System prompt >15K tokens (room for optimization)
- 🟢 System prompt <10K tokens (lean)

### 2.3 Tool-Conditional Guidance Audit

Astra injects guidance blocks based on which tools are available:
- Memory tools present → memory usage instructions
- GitHub tools present → PR/issue workflow guidance
- Git tools present → branching, commit message format
- Code navigation tools → how to explore code
- Glob/grep tools → search strategy

Check: Are all guidance blocks relevant to the current task?
- If task is "fix a typo in README.md", do we need GitHub PR guidance? Memory instructions?
- Each unnecessary guidance block wastes ~200-500 tokens.

### 2.4 Skill Injection Analysis

From journal `selected_skills` fields and checkpoint system messages:

```
| Turn | Skills Injected | Tokens Added | Referenced in Output? |
|------|----------------|-------------|----------------------|
```

Flag:
- 🔴 Skill injected but never referenced in assistant output (pure waste)
- 🟡 Same skill injected every turn (should be cached, not re-injected)
- 📛 Multiple large skills injected simultaneously (>3K tokens total)

---

## Phase 3: Tool Schema Analysis

### 3.1 Schema Token Budget

Astra sends tool schemas (JSON function definitions) alongside messages. Each schema
costs tokens proportional to its JSON size.

**From `ToolRegistry::token_cost()`**: `schema_bytes / 4` (approximate).

From journal `tools_selected` per turn:

```
| Turn | Tools Selected | Est. Schema Tokens | Tools Actually Used | Waste% |
|------|---------------|-------------------|--------------------|---------| 
```

### 3.2 Pinned vs Dynamic Schemas

- **Pinned schemas**: Always sent (core tools like bash, read_file, etc.)
  - Count: typically 5-10 tools
  - Cost: ~2-4K tokens (always paid)
  
- **Dynamic schemas**: Selected by tool selector per turn
  - Count: varies (5-25 tools)
  - Cost: ~2-10K tokens

**Total schema cost per turn**: `pinned_tokens + dynamic_tokens`

Flag:
- 🔴 >30 tools selected (>12K schema tokens — significant context waste)
- 🟡 >20 tools selected (>8K tokens)
- 🟢 <15 tools selected (<6K tokens — well-optimized)

### 3.3 Tool Selection Accuracy

Compare `tools_selected` vs `tools_used`:

```
Accuracy = |tools_used| / |tools_selected|
```

| Accuracy | Assessment |
|----------|-----------|
| >70% | 🟢 Excellent — selector is precise |
| 50-70% | 🟡 Acceptable — some waste |
| <50% | 🔴 Poor — more than half the schemas are unused |

### 3.4 Per-Tool Schema Size

Some tools have much larger schemas than others. Identify the top token consumers:

```bash
# From a debug dump, extract tool definitions
cat <checkpoint_file> | python3 -c "
import json, sys
msgs = json.load(sys.stdin)
# Tool schemas are typically in the first system message or as 'tools' array
# Look for function definitions
" 
```

Flag tools with schemas >500 tokens — candidates for schema trimming at high budget pressure.

---

## Phase 4: Conversation History Analysis

### 4.1 History Token Profile

From heavy checkpoint, break down message types:

```
| Category | Count | Total Bytes | Est. Tokens | % of Context |
|----------|-------|-------------|-------------|-------------|
| System   | n     | bytes       | tokens      | %           |
| User     | n     | bytes       | tokens      | %           |
| Assistant| n     | bytes       | tokens      | %           |
| Tool     | n     | bytes       | tokens      | %           |
| TOTAL    | n     | bytes       | tokens      | 100%        |
```

### 4.2 Tool Result Bloat

Tool results (role=tool messages) are often the largest context consumers.

```bash
cat <checkpoint_file> | python3 -c "
import json, sys
msgs = json.load(sys.stdin)
tool_msgs = [(i, m) for i, m in enumerate(msgs) if m.get('role') == 'tool']
tool_msgs.sort(key=lambda x: len(json.dumps(x[1])), reverse=True)
print('Top 10 largest tool results:')
for idx, (i, m) in enumerate(tool_msgs[:10]):
    name = m.get('name', m.get('tool_call_id', '?'))
    size = len(json.dumps(m))
    content_preview = str(m.get('content', ''))[:80]
    print(f'  {idx+1}. msg[{i}] {name}: {size:,} bytes ({size//4:,} tokens)')
    print(f'     {content_preview}...')
"
```

Flag:
- 🔴 Single tool result >10K bytes (~2.5K tokens) — should be truncated
- 🔴 Same file read multiple times in history (redundant)
- 🟡 Tool result >5K bytes — consider summarization

### 4.3 Repeated Content Detection

Check if the same file path appears in multiple tool results:

```bash
cat <checkpoint_file> | python3 -c "
import json, sys, re
msgs = json.load(sys.stdin)
paths = {}
for i, m in enumerate(msgs):
    content = str(m.get('content', ''))
    # Look for file path patterns in tool results
    for p in re.findall(r'(?:^|\s)(/[^\s]+\.[a-z]+)', content):
        paths.setdefault(p, []).append(i)
for p, indices in sorted(paths.items(), key=lambda x: -len(x[1])):
    if len(indices) > 1:
        print(f'  {p}: appears in {len(indices)} messages (indices: {indices})')
"
```

### 4.4 Stale Reasoning Detection

Astra calls `strip_stale_reasoning()` to remove old `reasoning_content` fields.
Check if any survive:

```bash
cat <checkpoint_file> | python3 -c "
import json, sys
msgs = json.load(sys.stdin)
for i, m in enumerate(msgs):
    rc = m.get('reasoning_content')
    if rc and len(str(rc)) > 0:
        print(f'  msg[{i}] role={m.get(\"role\")}: reasoning_content = {len(str(rc))} chars')
"
```

Flag:
- 🔴 reasoning_content on messages older than 3 turns (should have been stripped)

---

## Phase 5: Budget Pressure Analysis

### 5.1 Compaction Tier Thresholds

Astra's budget pressure tiers (from `prompts/context.rs`):

| Tier | Token Ratio | Pressure | Action |
|------|-------------|----------|--------|
| Normal | <60% | 0.0 | No action |
| TrimSchemas | 60-75% | 0.3 | Reduce dynamic tool schemas |
| CompactHistory | 75-85% | 0.6 | Compact older turns, keep recent 6 |
| AggressivePrune | >85% | 0.9 | Aggressive pruning, summarize history |

### 5.2 Budget Pressure Timeline

From journal `budget_pressure` fields:

```
| Turn | Tokens In | Budget Pressure | Tier | Action Taken |
|------|-----------|-----------------|------|-------------|
```

Healthy patterns:
- 🟢 **Sawtooth**: pressure rises gradually, drops after compaction
- 🟢 **Flat low**: stays below 0.3 (context fits comfortably)

Unhealthy patterns:
- 🔴 **Monotonic rise**: pressure increases every turn, never drops
- 🔴 **Sustained high**: pressure ≥0.6 for 5+ consecutive turns
- 🔴 **Oscillating high**: rapid 0.9→0.0→0.9 (aggressive compact then immediate refill)

### 5.3 Compaction Effectiveness

From journal `"type":"Compact"` events:
- How many turns were compacted?
- How many facts were extracted?
- What was the token reduction?

After compaction:
- Did `tokens_in` actually decrease on the next turn?
- Did the agent lose important context? (asks questions already answered)

### 5.4 Model Budget Utilization

```
Model context window: {model_limit} tokens
Output reserve (15%): {reserve} tokens
Effective input limit: {effective} tokens
Avg tokens_in: {avg} tokens ({avg/effective * 100}% utilization)
Peak tokens_in: {peak} tokens ({peak/effective * 100}% utilization)
```

---

## Phase 6: Optimization Report

```
╔══════════════════════════════════════════════════════════════╗
║  📐 Prompt Optimization Report                               ║
║  Session: {session_id} | Model: {model}                      ║
║  Context Window: {limit}K | Effective: {effective}K          ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  📊 Token Budget Breakdown (avg per turn)                    ║
║  ├─ System prompt:    {n}K tokens  ({pct}%)  {bar}          ║
║  ├─ Tool schemas:     {n}K tokens  ({pct}%)  {bar}          ║
║  ├─ Conversation:     {n}K tokens  ({pct}%)  {bar}          ║
║  ├─ Tool results:     {n}K tokens  ({pct}%)  {bar}          ║
║  ├─ Skills/memory:    {n}K tokens  ({pct}%)  {bar}          ║
║  └─ Available:        {n}K tokens  ({pct}%)  {bar}          ║
║                                                              ║
║  🎯 Optimization Score: {score}/100                          ║
║  ├─ System prompt efficiency: {s}/20                         ║
║  ├─ Tool selection precision: {s}/25                         ║
║  ├─ History management:       {s}/25                         ║
║  ├─ Budget pressure mgmt:     {s}/15                         ║
║  └─ Skill/memory efficiency:  {s}/15                         ║
║                                                              ║
║  💰 Token Savings Opportunities                              ║
║  ├─ Remove unused tool schemas: ~{n}K tokens/turn            ║
║  ├─ Truncate bloated tool results: ~{n}K tokens              ║
║  ├─ Remove redundant file reads: ~{n}K tokens                ║
║  ├─ Strip stale reasoning: ~{n}K tokens                      ║
║  └─ Reduce skill injections: ~{n}K tokens/turn               ║
║                                                              ║
║  TOTAL POTENTIAL SAVINGS: ~{n}K tokens/turn ({pct}%)         ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

---

## Optimization Recommendations by Component

### System Prompt
| Issue | Fix | Savings |
|-------|-----|---------|
| Unused tool guidance blocks | Remove guidance for tools not in current session | 200-500 tokens |
| Verbose core rules | Compress instruction language | 100-300 tokens |
| Large project profile | Summarize workspace state | 200-500 tokens |

### Tool Schemas
| Issue | Fix | Savings |
|-------|-----|---------|
| Low selection accuracy (<50%) | Strengthen TF-IDF signals (routing, entity graph, patterns, boost terms); tighten tool budget / thresholds — CLI does not use an LLM tool pre-selector | 2-5K tokens |
| >25 tools selected | Tighten TF-IDF threshold | 2-4K tokens |
| Large individual schemas | Trim verbose parameter descriptions | 100-500 tokens/tool |

### History
| Issue | Fix | Savings |
|-------|-----|---------|
| Tool result >10K bytes | Auto-truncate large results | 1-5K tokens |
| Same file read 3+ times | Deduplicate in history compaction | 1-3K tokens |
| Stale reasoning_content | Fix strip_stale_reasoning() | 500-2K tokens |

### Budget Pressure
| Issue | Fix | Savings |
|-------|-----|---------|
| Never compacts (pressure stays low) | Lower compact_threshold if context growing | Prevents future bloat |
| Too frequent compaction | Use larger model or reduce tool count | Preserves context quality |
| Oscillating high pressure | Task too complex for context window | Split into subtasks |

---

## Reference: Key Source Files

| Component | File |
|-----------|------|
| System prompt builder | `rust/crates/runtime/src/prompts/system.rs` |
| Context budget & tiers | `rust/crates/runtime/src/prompts/context.rs` |
| Token estimation | `rust/crates/runtime/src/prompts/context.rs` (`estimate_tokens_precise`) |
| Budget pressure calc | `rust/crates/runtime/src/turn/chat_turn_budget_pressure.rs` |
| Payload assembly | `rust/crates/runtime/src/turn/chat_turn_payload.rs` |
| Tool registry & costs | `rust/crates/runtime/src/tool_registry/registry.rs` |
| Tool selector | `rust/crates/runtime/src/tool_selector.rs` |
| Compaction | `rust/crates/runtime/src/turn/cloud/compaction.rs` |
| History management | `rust/crates/runtime/src/turn/history.rs` |

---

## Machine-Readable Output (auto-invoke)

When auto-invoked by [`AutoInvokeGate`](../../rust/crates/astra-skills/src/auto_invoke.rs), append a fenced JSON block at the end of your response matching the `SkillDiagnosis` schema:

````markdown
```skill-diagnosis
{
  "schema_version": 2,
  "skill": "optimize_prompt",
  "cause": "budget_pressure",
  "headline": "system prompt at 87% pressure — tool schemas dominate",
  "findings": [
    "12K tokens of tool schemas, ~30% are never called in this scenario",
    "reasoning_content persists across 5 turns without pruning"
  ],
  "recommended_action": "trim tool list to top-8 by recent use; drop stale reasoning_content",
  "success_criteria": [
    {
      "metric": "budget_pressure",
      "operator": "lte",
      "threshold": 0.85,
      "window_turns": 3,
      "description": "budget pressure returns below the auto-invoke threshold"
    }
  ],
  "source": "real_skill"
}
```
````

**Contract (enforced by `SkillDiagnosis::parse_from_skill_output`):**

- `schema_version` must be `2`.
- `cause` must be one of `session_stalls` | `budget_pressure` | `repeated_corrections`.
- `skill` should match `optimize_prompt`.
- `headline` ≤160 chars; `findings` ≤5 × ≤160 chars; `recommended_action` optional ≤160 chars.
- `success_criteria` is required and non-empty; use known metric/operator tags with finite thresholds and positive windows.
- `source` must be `real_skill` for actual skill output.
- Last block wins if multiple are present.

Keep the human-readable optimization report above the block for the interactive user.

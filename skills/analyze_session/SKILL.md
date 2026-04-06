---
name: analyze-session
description: "Developer skill: deep diagnostic analysis of an astra session — context quality, tool/skill selection accuracy, token efficiency, error patterns, compaction events, and stall detection. Reads from session journal (~/.astra/sessions/) and debug snapshots (/tmp/debug-*.json)."
user_invocable: true
when_to_use: "When the user wants to analyze a past session for token waste, tool selection accuracy, or context efficiency"
arguments:
  - name: TARGET
    description: "Session ID, debug JSON path (/tmp/debug-*.json), or keyword ('this', 'last'). Omit to analyze most recent."
    required: false
  - name: FOCUS
    description: "Analysis focus: 'context', 'tools', 'tokens', 'errors', 'flow', or 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Analyze Astra Session

Deep diagnostic analysis of an astra agent session. Uses the session journal (JSONL),
heavy checkpoints (full message snapshots), and debug dumps to identify inefficiencies,
bad tool choices, context bloat, error cascades, and compaction issues.

## Task

$ARGUMENTS

---

## Phase 1: Locate and Load Session Data

### 1.1 Resolve the TARGET

| TARGET type | Action |
|-------------|--------|
| File path (`/tmp/debug-*.json`) | Read directly — it's an array of OpenAI-style messages |
| UUID or short ID | Find journal at `~/.astra/sessions/<id>.jsonl` |
| `"this"` / `"current"` | Use current session's ID |
| `"last"` / `"previous"` | Pick the most recently modified `.jsonl` file |
| Omitted | Same as `"last"` |

### 1.2 Load the Session Journal (primary data source)

```bash
# List available sessions (most recent first)
ls -lt ~/.astra/sessions/*.jsonl 2>/dev/null | head -10

# Read the target session journal (JSONL — one JournalEvent per line)
cat ~/.astra/sessions/<SESSION_ID>.jsonl
```

Each line is a JSON object with this schema:
```
{
  "type": "Turn" | "TurnError" | "SessionStart" | "SessionEnd" | "Compact" |
          "ConfigChange" | "Error" | "StallDetected" | "Checkpoint" |
          "TurnGuardVerdict" | "PlanProgress" | "DelegationStarted" |
          "DelegationSubRunCompleted" | "DelegationCompleted" |
          "VerificationCompleted" | "CompositeSnapshot",
  "ts": "ISO-8601",
  "session_id": "uuid",
  "turn": <number>,
  "model": "<model_name>",
  "user_input": "<truncated 500 chars>",
  "assistant_output": "<truncated 1000 chars>",
  "tool_count": <n>,
  "tokens_in": <prompt_tokens>,
  "tokens_out": <completion_tokens>,
  "duration_ms": <ms>,
  "ttft_ms": <time_to_first_token_ms>,
  "tools_selected": ["<tools sent to LLM>"],
  "selected_skills": ["<skills injected>"],
  "tools_used": ["<tools actually called by LLM>"],
  "tool_calls": [
    {
      "name": "<tool>",
      "ok": true|false,
      "ms": <duration>,
      "error": "<message if failed>",
      "input_bytes": <n>,
      "output_bytes": <n>,
      "args_preview": "<~80 chars>",
      "result_preview": "<~500 chars>"
    }
  ],
  "budget_used": <tool_token_budget>,
  "budget_pressure": <0.0 to 0.9>,
  "selector_strategy": "tfidf|llm|...",
  "selector_ms": <tool_selection_time>,
  "context_ms": <prompt_building_time>,
  "stall_type": "sig_stall|name_stall|divergence|null",
  "error": "<error_message>",
  "plan_subtask_id": "<if plan mode>"
}
```

### 1.3 Load Heavy Checkpoints (for message-level analysis)

```bash
# List checkpoints for this session
ls ~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json 2>/dev/null
```

Each heavy checkpoint is a JSON array of OpenAI-format messages (role/content/tool_calls).
Use these to inspect what the LLM actually saw at each turn.

### 1.4 Load Debug Snapshots (if TARGET is a file)

Debug files from `/debug` REPL command come in two schemas:
- **`astra-debug-turn-delta-v1`**: Only the messages added in this turn
- **`astra-debug-turn-full-v1`**: Full message array at this point

Look for the `"schema"` field, or if absent, it's a raw message array.
The `"journal_turn_summary"` object inside contains pre-computed metrics.

---

## Phase 2: Turn-by-Turn Metrics Extraction

Parse every `"type":"Turn"` and `"type":"TurnError"` event from the journal.
Build a summary table:

```
| Turn | Model | Tokens In | Tokens Out | TTFT | Duration | Tools Selected | Tools Used | Errors | Budget Pressure | Strategy |
|------|-------|-----------|------------|------|----------|----------------|------------|--------|-----------------|----------|
```

Compute aggregates:
- **Total turns**, **total tokens** (in + out), **total duration**
- **Avg tokens per turn** (prompt & completion separately)
- **Avg TTFT** (time to first token — measures LLM responsiveness)
- **Avg context_ms** (prompt building time — measures our overhead)
- **Tool selection accuracy**: `|tools_used| / |tools_selected|` — low means we're sending too many tool schemas
- **Tool utilization**: % of turns that actually called tools

---

## Phase 3: Tool Selection Analysis

### 3.1 Tool Selection Accuracy

For each turn, compare `tools_selected` (what we sent) vs `tools_used` (what LLM called):
- **Wasted slots**: tools in `selected` but not in `used` — burning tokens on unused schemas
- **Missing tools**: if LLM tried to call a tool not in `selected`, our selector missed it
- **Selector strategy**: was `tfidf` or `llm` used? Track per-turn. Note switches.
- **Selector time**: `selector_ms` — is tool selection itself slow?

### 3.2 Per-Tool Audit

From `tool_calls` arrays across all turns, build:

```
| Tool | Total Calls | Success% | Avg Duration | Avg Input | Avg Output | Failure Pattern |
|------|-------------|----------|--------------|-----------|------------|-----------------|
```

Flag:
- 🔴 Tools with >30% failure rate
- 🟡 Tools with avg output >5000 bytes (bloating context)
- 🟡 Same tool called >5 times in one turn (stuck in retry loop)
- 📛 `bash` calls where `grep`/`glob`/`read_file` would suffice
  (check `args_preview` for patterns like `grep`, `find`, `cat`, `ls`)

### 3.3 Skill Injection Analysis

From `selected_skills` across turns:
- Which skills were injected and how often?
- Were they relevant to the user's task? (compare with `user_input`)
- Did skill injection increase prompt tokens significantly?

### 3.4 Parallelism Analysis

Examine consecutive `tool_calls` entries within the same turn:
- If multiple read-only tools (read_file, grep, glob) were called sequentially,
  they could have been parallel. Count missed opportunities.
- Check the PARALLEL_SAFE_TOOLS set (34 tools are safe for parallel pre-execution).
  Mutating tools (bash, write_file, str_replace, git_commit, memory_store) force sequential.

---

## Phase 4: Context & Compaction Analysis

### 4.1 Token Growth Curve

Plot (as text table) how `tokens_in` grows across turns:
```
| Turn | Prompt Tokens | Δ from Previous | Growth Rate |
|------|---------------|-----------------|-------------|
```

Flag:
- 🔴 **Exponential growth**: prompt tokens doubling every 2-3 turns → history not compacted
- 🟡 **Sudden jumps**: >50% increase in one turn → huge tool result or file read
- 🟢 **Sawtooth pattern**: grows then drops → compaction is working

### 4.2 Compaction Events

Filter journal for `"type":"Compact"` events:
- `turns_compacted`: how many turns were summarized
- `facts_stored`: how many facts extracted during compaction
- `budget_pressure`: what tier triggered it (0.3 = trim, 0.6 = compact, 0.9 = aggressive)

Flag:
- 🔴 No compaction events but tokens_in >100k → compaction not triggering
- 🟡 Very frequent compaction (every 2-3 turns) → context window too small for task
- 🟡 `budget_pressure` ≥ 0.9 → aggressive compaction, likely losing context

### 4.3 Heavy Checkpoint Analysis (if available)

Read 1-2 heavy checkpoint files and analyze the actual message array:
- **System message size**: Count tokens in first message (role=system)
- **Tool result sizes**: For each role=tool message, note content length
- **Repeated content**: Same file path appearing in multiple tool results
- **Stale reasoning**: Old `reasoning_content` fields surviving
  (astra calls `strip_stale_reasoning()` — check if it's working)

---

## Phase 5: Error & Stall Analysis

### 5.1 Error Events

Filter journal for `"type":"TurnError"`, `"type":"Error"`, `"type":"StallDetected"`:

```
| Turn | Type | Error Message | Stall Type | Recovery |
|------|------|---------------|------------|----------|
```

### 5.2 Stall Detection

For `StallDetected` events, check `stall_type`:
- **`sig_stall`**: Tool signature repetition (same tool called repeatedly)
- **`name_stall`**: Same tool names repeating without progress
- **`divergence`**: Agent output diverging from task goal

Did the stall detector intervene correctly? Check subsequent turns:
- Did the agent change approach after stall detection?
- Or did it continue the same pattern?

### 5.3 TurnGuardVerdict Events

Non-happy-path audit events. Check the metadata for:
- What guard triggered (token limit, safety, etc.)
- What action was taken (truncate, reject, warn)

### 5.4 Error Cascades

Look for patterns where one error leads to subsequent errors:
- Turn N: tool failure → Turn N+1: LLM retries same approach → Turn N+2: more failures
- Compaction lost context → LLM confused → wrong tool calls → errors

### 5.5 Tool Failure Analysis

From `tool_calls` with `ok: false`:
- Group by tool name and error pattern
- Identify systemic issues (e.g., all `bash` calls failing = environment issue)
- Check if `error` field contains actionable info or is generic

---

## Phase 6: Execution Flow Analysis

### 6.1 User Interaction Pattern

From `user_input` fields across Turn events:
- How many user messages?
- Are user messages getting shorter (trust building) or longer (agent not understanding)?
- Any repeated clarifications?

### 6.2 Plan Mode Analysis

If `plan_subtask_id` is set on any turns:
- Track which subtask each turn belongs to
- Filter for `"type":"PlanProgress"` events
- Were subtasks completed in dependency order?
- Any subtask retries or failures?

### 6.3 Delegation Analysis

If `DelegationStarted`/`DelegationCompleted` events exist:
- How many sub-runs were spawned?
- Fan-out pattern (sequential, parallel, adversarial)?
- Any sub-run failures?
- Was delegation result quality verified?

### 6.4 Turn Efficiency Classification

Classify each turn:
- **Productive**: Made visible progress (code written, test passed, file created)
- **Exploratory**: Gathered information needed for progress (read files, searched)
- **Recovery**: Fixed a mistake from a previous turn
- **Wasted**: No meaningful progress, redundant work, circular reasoning
- **Blocked**: Waiting for user input or external resource

---

## Phase 7: Diagnostic Report

```
╔══════════════════════════════════════════════════════════════╗
║  🔬 Astra Session Diagnostic                                ║
║  Session: {session_id}                                       ║
║  Model: {model} | Turns: {n} | Duration: {total_time}       ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  📊 Overview                                                 ║
║  ├─ User messages: {n}                                       ║
║  ├─ LLM turns: {n} (productive: {n}, wasted: {n})           ║
║  ├─ Tool calls: {n} (success: {n}%, parallel: {n}%)         ║
║  ├─ Total tokens: {in}+{out} = {total}                      ║
║  ├─ Avg TTFT: {ms}ms | Avg context build: {ms}ms            ║
║  └─ Errors: {n} | Stalls: {n} | Compactions: {n}            ║
║                                                              ║
║  🎯 Health Score: {score}/100                                ║
║  ├─ Context efficiency:   {score}/25  {bar}                  ║
║  ├─ Tool selection:       {score}/25  {bar}                  ║
║  ├─ Token efficiency:     {score}/25  {bar}                  ║
║  └─ Error handling:       {score}/25  {bar}                  ║
║                                                              ║
║  🔴 Critical ({n})                                           ║
║  {issues}                                                    ║
║                                                              ║
║  🟡 Warnings ({n})                                           ║
║  {warnings}                                                  ║
║                                                              ║
║  💡 Recommendations                                          ║
║  {numbered_recommendations}                                  ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

### Scoring Rubric

**Context Efficiency (25 pts)**:
- 10 pts: Token growth linear or sawtooth (compaction working)
- 5 pts: No repeated file reads
- 5 pts: Tool results not bloated (avg <2000 bytes)
- 5 pts: System prompt reasonable size

**Tool Selection (25 pts)**:
- 10 pts: Selection accuracy >70% (tools_used/tools_selected)
- 5 pts: No `bash` misuse (using bash for grep/find/cat)
- 5 pts: Parallelism utilized (>50% of parallel-safe batches)
- 5 pts: No blind retries (same failed tool call repeated)

**Token Efficiency (25 pts)**:
- 10 pts: Avg prompt tokens per turn <30k
- 5 pts: Completion/prompt ratio between 5%-50%
- 5 pts: No turns with >100k prompt tokens
- 5 pts: Selector overhead <500ms avg

**Error Handling (25 pts)**:
- 10 pts: Tool success rate >90%
- 5 pts: No unrecovered errors
- 5 pts: No stall cascades (stall → recovery within 2 turns)
- 5 pts: No TurnGuardVerdict escalations

---

## Astra-Specific Anti-Patterns

### Context Anti-Patterns
- 📛 **No compaction**: `budget_pressure` stays at 0 but tokens grow past 80k
- 📛 **Aggressive compaction loop**: `budget_pressure` ≥ 0.9 for >3 consecutive turns
- 📛 **Stale reasoning**: `reasoning_content` from old turns surviving (`strip_stale_reasoning` failing)
- 📛 **Tool schema bloat**: `tools_selected` has >30 tools (each schema ~500 tokens)

### Tool Anti-Patterns
- 📛 **Selector miss**: LLM tried a tool not in `tools_selected`
- 📛 **Strategy mismatch**: `tfidf` for novel task where LLM selection would be better
- 📛 **Skill injection waste**: Skill injected but never referenced in output
- 📛 **Output explosion**: Single tool result >10000 bytes inflating context

### Flow Anti-Patterns
- 📛 **Stall ignored**: `StallDetected` event followed by same tool pattern
- 📛 **Plan drift**: `plan_subtask_id` changes mid-subtask without completion
- 📛 **Delegation waste**: Sub-run spawned but result not used

### Performance Anti-Patterns
- 📛 **Slow context build**: `context_ms` >2000ms (prompt assembly bottleneck)
- 📛 **Slow selector**: `selector_ms` >1000ms (tool selection bottleneck)
- 📛 **High TTFT**: `ttft_ms` >10000ms (LLM latency issue)
- 📛 **Long turns**: `duration_ms` >120000ms without tool calls (LLM stalling)

---

## Reference: Key Source Files

When investigating issues found by this analysis, relevant astra source files:

| Component | File |
|-----------|------|
| Session journal | `rust/crates/services/src/session_journal.rs` |
| Event ingestion (cloud) | `rust/crates/services/src/event_ingestion.rs` |
| Tool selection | `rust/crates/runtime/src/turn/tool_selection.rs` |
| History management | `rust/crates/runtime/src/turn/history.rs` |
| Compaction | `rust/crates/runtime/src/turn/cloud/compaction.rs` |
| Edge ledger | `rust/crates/runtime/src/turn/edge_ledger.rs` |
| Stall detection | `rust/crates/runtime/src/stall_detector.rs` |
| Chat stream (main loop) | `rust/crates/astra-cli/src/cli/chat_stream.rs` |
| Plan executor | `rust/crates/astra-cli/src/cli/plan_executor.rs` |
| Debug inspector | `rust/crates/astra-cli/src/cli/slash_debug.rs` |
| REPL turn handler | `rust/crates/astra-cli/src/cli/repl_turn.rs` |

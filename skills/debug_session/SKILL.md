---
name: debug-session
description: "Developer skill: diagnose why an astra session is stuck, slow, or producing bad results. Analyzes stall patterns, turn guard verdicts, error cascades, tool health, and escalation levels using journal + checkpoint data."
user_invocable: true
arguments:
  - name: TARGET
    description: "Session ID, debug JSON path, or keyword ('this', 'last'). Omit for most recent."
    required: false
  - name: SYMPTOM
    description: "What's wrong: 'stuck', 'slow', 'looping', 'wrong-tools', 'errors', or 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Debug Astra Session

Diagnose why an astra session is stuck, slow, looping, or producing bad results.
This skill goes deeper than `analyze-session` — it focuses on **root cause diagnosis**
by mapping observed symptoms to astra's internal correction mechanisms.

## Task

$ARGUMENTS

---

## Phase 1: Load Session Data

### 1.1 Resolve TARGET (same as analyze-session)

| TARGET type | Action |
|-------------|--------|
| File path (`/tmp/debug-*.json`) | Read directly |
| UUID or short ID | Journal at `~/.astra/sessions/<id>.jsonl` |
| `"this"` / `"last"` / omitted | Most recent `.jsonl` file |

```bash
ls -lt ~/.astra/sessions/*.jsonl 2>/dev/null | head -5
```

### 1.2 Load Primary Data

```bash
# Journal (JSONL)
cat ~/.astra/sessions/<SESSION_ID>.jsonl

# Heavy checkpoints (full message arrays)
ls ~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json 2>/dev/null
```

---

## Phase 2: Symptom Detection

Parse the journal and classify the session's problems. Check ALL of these even if SYMPTOM
is specified — problems compound.

### 2.1 Stall Pattern Analysis

Astra's stall detector (`turn/stall.rs`) recognizes three patterns:

| Stall Type | Pattern | Journal Field |
|------------|---------|---------------|
| **sig_stall** | Same tool signature (name + args shape) repeated ≥3 times | `stall_type: "sig_stall"` |
| **name_stall** | Same tool names repeating across turns | `stall_type: "name_stall"` |
| **divergence** | Only exploration tools used for N+ rounds, no progress | `stall_type: "divergence"` |

Extract from journal:
```
Filter: type == "StallDetected" OR stall_type != null
```

For each stall event, check:
1. **Was the nudge effective?** Compare tools_used in the stall turn vs next 2 turns
   - If same tools reappear → nudge was ignored → escalation should follow
2. **Did the agent change approach?** Look at `tools_selected` changes
3. **How many nudges total?** Count stall events — each nudge reduces confidence

### 2.2 Turn Guard Escalation Analysis

Astra's `TurnGuard` (`turn/turn_guard.rs`) has 3 escalation levels:

| Level | Trigger | Action |
|-------|---------|--------|
| **Normal** | nudge_count < 2 AND total_errors < 5 | Hints only |
| **Warning** | nudge_count ≥ 2 OR total_errors ≥ 5 | Explicit tool avoidance |
| **Critical** | nudge_count ≥ 3 + errors ≥ 2, OR errors ≥ 8 + deprioritized, OR errors ≥ 10 | Restrict to read-only, force stop on 2nd critical |

Extract from journal:
```
Filter: type == "TurnGuardVerdict"
```

Check the escalation trajectory:
- Normal → Warning → Critical = proper escalation
- Stuck at Warning for 5+ turns = guard not escalating enough
- Jump straight to Critical = catastrophic failure cascade

### 2.3 Tool Health Degradation

From `tool_calls` arrays across turns, build per-tool health:

```
| Tool | Calls | Consecutive Fails | Deprioritized? | Timeout-Dominant? |
|------|-------|-------------------|----------------|-------------------|
```

Astra's `ToolHealthTracker` (`turn/tool_health.rs`) tracks:
- **Consecutive failures** → deprioritize after threshold
- **Timeout-dominant** tools → suggest alternatives
- **Rehabilitation** → tool un-deprioritized after 3 successes

Flag:
- 🔴 Tool deprioritized but still being called (TurnGuard override not working)
- 🔴 Same tool failing >5 times consecutively
- 🟡 Tool with >50% timeout rate

### 2.4 Error Cascade Detection

Astra's `SessionErrorSummary` (`turn/error_recovery.rs`) categorizes errors:

```
Filter: type == "TurnError" OR type == "Error" OR tool_calls[].ok == false
```

Build an error timeline:
```
| Turn | Error Source | Error Message | Recovery Action | Recovered? |
|------|-------------|---------------|-----------------|------------|
```

Look for cascades:
- **Error → Retry → Same Error** = blind retry (no adaptation)
- **Error → Different Error** = downstream failure (first error caused second)
- **Error → Compaction → Lost Context → More Errors** = compaction-induced amnesia
- **5+ errors in 3 turns** = catastrophic cascade

### 2.5 Latency Analysis

```
Filter: type == "Turn"
Extract: ttft_ms, duration_ms, context_ms, selector_ms
```

| Symptom | Threshold | Root Cause |
|---------|-----------|------------|
| High TTFT | >10s | LLM provider latency or huge prompt |
| High context_ms | >2s | Prompt assembly bottleneck (too many tools, large history) |
| High selector_ms | >1s | Tool selection bottleneck (LLM selector on large tool set) |
| High duration_ms with no tools | >120s | LLM generating very long response or stalling |

---

## Phase 3: Root Cause Diagnosis

Map observed symptoms to root causes using this decision tree:

```
Session stuck/looping?
├─ StallDetected events exist?
│  ├─ sig_stall → Agent repeating exact same tool calls
│  │  └─ Check: Is the tool actually failing? Or succeeding but agent not reading result?
│  ├─ name_stall → Agent cycling through same tool types
│  │  └─ Check: Are tool results useful? Or is context too compressed to remember them?
│  └─ divergence → Agent exploring endlessly without making progress
│     └─ Check: Does the plan have clear acceptance criteria? Or is goal ambiguous?
├─ No StallDetected but looping?
│  └─ Stall detector window too wide (default 6) — pattern repeats every 7+ turns
│
Session producing wrong results?
├─ Tool selection accuracy low? (<50% tools_used/tools_selected)
│  └─ Check: selector_strategy — was tfidf used for novel task?
├─ Skill injection issues?
│  └─ Check: selected_skills — irrelevant skills bloating context?
├─ Compaction lost critical context?
│  └─ Check: tokens_in drops sharply then agent asks questions already answered
│
Session slow?
├─ High TTFT? → LLM provider issue or prompt too large
├─ High context_ms? → Too many tools selected (check tools_selected count)
├─ High selector_ms? → Switch to tfidf or reduce tool pool
├─ Long tool execution? → Check tool_calls[].ms for outliers
└─ Frequent compaction? → Context window too small for task complexity
```

---

## Phase 4: Correction Record Analysis

Astra's TurnGuard generates `CorrectionRecord` entries when it intervenes:

```rust
CorrectionRecord {
    turn: u32,
    correction_type: String,  // "stall_nudge", "divergence", "deprioritize", "error_escalation"
    avoid_tools: Vec<String>,
    suggested_alternatives: Vec<String>,
}
```

For each correction in the journal:
1. Was the correction followed? (agent used alternatives instead of avoided tools)
2. Did the correction help? (error rate decreased, new tools used)
3. Was the correction appropriate? (sometimes the "avoided" tool is actually needed)

Build a correction effectiveness table:
```
| Turn | Correction Type | Avoid Tools | Followed? | Effective? |
|------|----------------|-------------|-----------|------------|
```

### Correction Outcome Classification

- ✅ **Effective**: Agent changed approach, problem resolved within 2 turns
- ⚠️ **Partially effective**: Agent changed tools but problem persisted
- ❌ **Ignored**: Agent continued with same tools despite nudge
- 🔄 **Wrong correction**: Avoided tool was actually needed (false positive stall)

---

## Phase 5: Checkpoint Forensics (if available)

When heavy checkpoints exist, do deep message-level analysis:

### 5.1 Context Window Forensics

Read the most recent heavy checkpoint and analyze:
- **System message size**: How many tokens is the system prompt?
- **Tool schemas present**: Count tool definitions in system message
- **History depth**: How many assistant/user/tool turns in context?
- **Largest tool result**: Which tool result is consuming the most tokens?
- **Repeated content**: Same file path appearing multiple times
- **Stale reasoning**: Old `reasoning_content` fields that should have been stripped

### 5.2 Message Delta Analysis

If multiple checkpoints exist, compare adjacent ones:
- What messages were added between checkpoints?
- Were old messages compacted (summarized)?
- Did compaction lose critical information?

### 5.3 Nudge Message Inspection

Astra injects nudge messages as system-role messages. Find them:
```
Filter: role == "system" AND content contains "STALL" or "DIVERGENCE" or "avoid"
```

Check:
- Is the nudge clearly worded?
- Does it provide specific alternatives?
- Is the agent's next response acknowledging the nudge?

---

## Phase 6: Diagnostic Report

```
╔══════════════════════════════════════════════════════════════╗
║  🔧 Astra Session Debug Report                              ║
║  Session: {session_id}                                       ║
║  Symptom: {primary_symptom}                                  ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  🎯 Root Cause: {one-line diagnosis}                         ║
║                                                              ║
║  📋 Evidence Chain:                                          ║
║  ├─ Turn {n}: {first symptom observed}                       ║
║  ├─ Turn {n}: {escalation/cascade}                           ║
║  ├─ Turn {n}: {correction attempted}                         ║
║  └─ Turn {n}: {current state}                                ║
║                                                              ║
║  ⚙️ Internal State:                                         ║
║  ├─ Escalation level: {Normal/Warning/Critical}              ║
║  ├─ Stall nudges sent: {n}                                   ║
║  ├─ Tools deprioritized: {list}                              ║
║  ├─ Consecutive errors: {n}                                  ║
║  └─ Correction effectiveness: {n}/{total} followed           ║
║                                                              ║
║  🔴 Primary Issue                                            ║
║  {detailed explanation of root cause}                        ║
║                                                              ║
║  🟡 Contributing Factors                                     ║
║  {secondary issues that made the primary worse}              ║
║                                                              ║
║  💊 Recommended Fix                                          ║
║  {specific actions to take — which code to change, which     ║
║   thresholds to adjust, which tool to add/remove}            ║
║                                                              ║
║  📁 Relevant Source Files                                    ║
║  {files to investigate based on diagnosis}                   ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

---

## Common Diagnoses & Fixes

| Diagnosis | Fix |
|-----------|-----|
| Stall detector window too wide | Reduce `STALL_WINDOW` in `stall.rs` |
| Nudge ignored by LLM | Strengthen nudge language in `build_stall_reflection()` |
| Tool deprioritized incorrectly | Adjust consecutive failure threshold in `tool_health.rs` |
| Compaction too aggressive | Lower budget_pressure thresholds in compaction config |
| Selector picking wrong strategy | Force `llm` strategy for novel tasks in `tool_selection.rs` |
| Context window too small | Increase model context or reduce tool schema count |
| Error recovery not escalating | Adjust thresholds in `error_recovery.rs` EscalationLevel |

---

## Reference: Key Source Files

| Component | File |
|-----------|------|
| Stall detection | `rust/crates/runtime/src/turn/stall.rs` |
| Turn guard & verdicts | `rust/crates/runtime/src/turn/turn_guard.rs` |
| Error recovery & escalation | `rust/crates/runtime/src/turn/error_recovery.rs` |
| Tool health tracking | `rust/crates/runtime/src/turn/tool_health.rs` |
| Session journal | `rust/crates/services/src/session_journal.rs` |
| Debug command | `rust/crates/mo-agent/src/mo_agent/slash_debug.rs` |
| Chat stream (main loop) | `rust/crates/mo-agent/src/mo_agent/chat_stream.rs` |
| Compaction | `rust/crates/runtime/src/turn/cloud/compaction.rs` |

---
name: analyze-session
description: "Developer skill: diagnostic analysis and debugging of an astra session. Primary input is `astra journal digest` (stable JSON from local ~/.astra/sessions). Optional deep dive: heavy checkpoints, debug JSON, stall/escalation forensics."
user_invocable: true
when_to_use: "When the user wants to analyze a past session for token waste, tool selection accuracy, context efficiency, or diagnose why a session is stuck, slow, or looping"
arguments:
  - name: TARGET
    description: "Session ID, debug JSON path (/tmp/debug-*.json), or keyword ('this', 'last'). Omit to analyze most recent."
    required: false
  - name: FOCUS
    description: "Interpretation focus: 'context', 'tools', 'tokens', 'errors', 'flow', 'debug', or 'all' (default: all)"
    required: false
  - name: SYMPTOM
    description: "For debug focus: 'stuck', 'slow', 'looping', 'wrong-tools', 'errors', or 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Analyze Astra Session

Use **machine-generated metrics first** (`astra journal digest`), then interpret. Do not re-parse raw JSONL or recompute aggregates by hand unless digest is unavailable.

## Task

$ARGUMENTS

---

## Phase 1: Load digest (required)

Run from the project or any directory (command is offline; no login required). If `astra` is not on `PATH`, use the full path to the CLI binary (ask the user or use `command -v astra`).

```bash
# Most recent local session
astra journal digest

# Specific session (full UUID, unique prefix, or last)
astra journal digest <SESSION_ID>
astra journal digest --session <SESSION_ID>
astra journal digest last

# Smaller JSON (metrics-focused turn rows)
astra journal digest --focus summary

# Human-readable
astra journal digest --format text
```

**Stable schema**: root field `schema_version` is `astra-journal-digest-v1`. If it differs, describe the mismatch and still use whatever fields exist.

### JSON fields to use (do not invent numbers)

- **`journal_lines_non_empty`**, **`journal_lines_malformed`**: raw JSONL line counts; non-zero `malformed` means some lines were skipped during parse.
- **`aggregates`**: `session_start_count`, `session_end_count`, `turn_count`, `turn_error_count`, `compact_count`, `stall_count`, `error_event_count`, `total_tokens_in`, `total_tokens_out`, `total_duration_ms`, `total_tool_calls`, `tool_calls_failed`, `avg_tokens_in`, `avg_tokens_out`, `avg_duration_ms`.
- **`turns`**: per-turn `seq` (1-based chronological index), `turn_id` (session turn counter when present), `tokens_in` / `tokens_out`, `duration_ms`, `ttft_ms`, `context_ms`, `selector_ms`, `selector_strategy`, `tools_selected_count`, `tools_used_count`, `selected_skills`, `tool_calls_ok`, `tool_calls_fail`, `user_input_preview`, `budget_pressure`.
- **`compaction_events`**, **`stalls`**, **`turn_errors`**, **`other_errors`**: structured side events; cite `ts`, `turn`, and `detail` from JSON.
- **`failed_tool_calls`**: per-call details for every failed tool call. Each entry has `seq` (turn sequence), `turn_id`, `tool`, `error_category` (`safety_guard` / `permission_denied` / `tool_error` / `unknown`), `error_preview` (first ~200 chars), `args_preview`. Only present in `--focus all` (default). Use this to identify false positives and error patterns without re-parsing raw JSONL.
- **`aggregates.safety_guard_blocks`**: count of tool calls blocked by a safety guard. Non-zero means the agent hit safety walls — check `failed_tool_calls` for details.

### Resolve TARGET when not using default session

| TARGET | Command |
|--------|---------|
| Omitted / `last` / `previous` | `astra journal digest` |
| UUID or short ID | `astra journal digest <id>` |
| `this` / `current` | Use active session id from user context; same as above with that id |
| Path `/tmp/debug-*.json` | Skip digest for metrics; go to Phase 3 only |

---

## Phase 2: Interpretation (by FOCUS)

Use **only** digest fields. Quote or paraphrase numbers from JSON; do not estimate token totals from prose.

**all**: Short narrative covering aggregates, token trend across `turns[].seq` (e.g. monotonic growth vs drops after `compaction_events`), tool health (`tool_calls_fail` vs `total_tool_calls`), stalls/errors.

**tokens**: Emphasize `avg_*`, per-turn `tokens_in`, and relation to `compact_count`.

**tools**: Compare `tools_selected_count` vs `tools_used_count`; `tool_calls_ok` / `tool_calls_fail`; `selector_strategy` / `selector_ms` when present.

**errors**: `turn_errors`, `other_errors`, `stalls`, `turn_error_count`; tie to neighboring `turns` by `turn` / `seq` when possible.

**flow**: User-visible cadence via `user_input_preview`, durations, and whether compactions cluster.

**context**: `context_ms`, `memoria_ms`, `ttft_ms`, `budget_pressure` patterns.

**debug**: Deep root-cause diagnosis — proceed to Phase 2D below.

---

## Phase 2D: Debug Diagnosis (FOCUS=debug)

When FOCUS is `debug`, perform deep root-cause analysis using journal data. Check ALL symptom categories even if SYMPTOM is specified — problems compound.

### 2D.1 Stall Pattern Analysis

Astra's stall detector (`turn/stall.rs`) recognizes three patterns:

| Stall Type | Pattern | Journal Field |
|------------|---------|---------------|
| **sig_stall** | Same tool signature (name + args shape) repeated ≥3 times | `stall_type: "sig_stall"` |
| **name_stall** | Same tool names repeating across turns | `stall_type: "name_stall"` |
| **divergence** | Only exploration tools used for N+ rounds, no progress | `stall_type: "divergence"` |

Extract from journal: `type == "StallDetected" OR stall_type != null`

For each stall event, check:
1. **Was the nudge effective?** Compare tools_used in the stall turn vs next 2 turns
2. **Did the agent change approach?** Look at `tools_selected` changes
3. **How many nudges total?** Count stall events — each nudge reduces confidence

### 2D.2 Turn Guard Escalation Analysis

Astra's `TurnGuard` (`turn/turn_guard.rs`) has 3 escalation levels:

| Level | Trigger | Action |
|-------|---------|--------|
| **Normal** | nudge_count < 2 AND total_errors < 5 | Hints only |
| **Warning** | nudge_count ≥ 2 OR total_errors ≥ 5 | Explicit tool avoidance |
| **Critical** | nudge_count ≥ 3 + errors ≥ 2, OR errors ≥ 8 + deprioritized, OR errors ≥ 10 | Restrict to read-only, force stop on 2nd critical |

Extract from journal: `type == "TurnGuardVerdict"`

Check the escalation trajectory:
- Normal → Warning → Critical = proper escalation
- Stuck at Warning for 5+ turns = guard not escalating enough
- Jump straight to Critical = catastrophic failure cascade

### 2D.3 Tool Health Degradation

From `tool_calls` arrays across turns, build per-tool health:

| Tool | Calls | Consecutive Fails | Deprioritized? | Timeout-Dominant? |
|------|-------|-------------------|----------------|-------------------|

Flags:
- 🔴 Tool deprioritized but still being called
- 🔴 Same tool failing >5 times consecutively
- 🟡 Tool with >50% timeout rate

### 2D.4 Error Cascade Detection

Build an error timeline from `type == "TurnError" OR type == "Error" OR tool_calls[].ok == false`:

| Turn | Error Source | Error Message | Recovery Action | Recovered? |
|------|-------------|---------------|-----------------|------------|

Cascade patterns:
- **Error → Retry → Same Error** = blind retry (no adaptation)
- **Error → Different Error** = downstream failure
- **Error → Compaction → Lost Context → More Errors** = compaction-induced amnesia
- **5+ errors in 3 turns** = catastrophic cascade

### 2D.5 Latency Analysis

| Symptom | Threshold | Root Cause |
|---------|-----------|------------|
| High TTFT | >10s | LLM provider latency or huge prompt |
| High context_ms | >2s | Prompt assembly bottleneck |
| High selector_ms | >1s | Tool selection bottleneck |
| High duration_ms with no tools | >120s | LLM generating very long response or stalling |

### 2D.6 Root Cause Decision Tree

```
Session stuck/looping?
├─ StallDetected events exist?
│  ├─ sig_stall → Agent repeating exact same tool calls
│  ├─ name_stall → Agent cycling through same tool types
│  └─ divergence → Agent exploring endlessly without progress
├─ No StallDetected but looping?
│  ├─ safety_guard_blocks > 0 AND user keeps saying "not working"?
│  │  └─ Verification gap: agent can't confirm output → rewrites from scratch
│  │     → Check Phase 2E for false positive patterns
│  └─ Stall detector window too wide (default 6)
│
Session producing wrong results?
├─ Tool selection accuracy low? (<50% tools_used/tools_selected)
├─ Skill injection issues? (irrelevant skills bloating context)
├─ Compaction lost critical context? (tokens_in drops then agent re-asks)
│
Session slow?
├─ High TTFT? → LLM provider issue or prompt too large
├─ High context_ms? → Too many tools selected
├─ High selector_ms? → Switch to tfidf or reduce tool pool
├─ Long tool execution? → Check tool_calls[].ms for outliers
└─ Frequent compaction? → Context window too small for task
```

### 2D.7 Correction Effectiveness

For each TurnGuard correction in the journal, evaluate:

| Turn | Correction Type | Avoid Tools | Followed? | Effective? |
|------|----------------|-------------|-----------|------------|

- ✅ **Effective**: Agent changed approach, resolved within 2 turns
- ⚠️ **Partially effective**: Changed tools but problem persisted
- ❌ **Ignored**: Same tools despite nudge
- 🔄 **Wrong correction**: Avoided tool was actually needed

---

## Phase 2E: Tool Failure Forensics (when `tool_calls_failed > 0`)

Run this phase whenever `aggregates.tool_calls_failed > 0`. It uses the `failed_tool_calls` array from the digest — no raw JSONL parsing needed.

### 2E.1 Safety Guard Analysis

If `aggregates.safety_guard_blocks > 0`:

1. List all `failed_tool_calls` where `error_category == "safety_guard"`.
2. For each, extract the guard name from `error_preview` (e.g. `shell_obfuscation`, `interpreter_stdin`).
3. Check `args_preview` to determine if the block was a **true positive** (genuinely dangerous) or **false positive** (legitimate command misclassified).

**Common false positive patterns**:
| Pattern | Example | Root Cause | Status |
|---------|---------|------------|--------|
| `grep 'foo\|bar'` in multi-segment command | `ls && grep 'a\|b' file` | `\|` matched `\|sh` substring in pipe-to-shell check | ✅ Fixed in `permission_manager.rs` |
| `node -e "..."` with JS template literals | `node -e "const x = \`hi\`"` | Backticks in double quotes = bash command substitution | ⚠️ By design — use single quotes |
| `python3 -c "..."` with `$()` | `python3 -c "page.$('sel')"` | `$()` in double quotes = bash command substitution | ⚠️ By design — use single quotes |

**If false positive**: note the guard name and args pattern. This is a bug to fix in `safety_middleware.rs` or `permission_manager.rs`.

**If true positive**: note what the agent was trying to do and suggest a safer alternative.

### 2E.2 Permission Denied Analysis

If any `failed_tool_calls` have `error_category == "permission_denied"`:

1. Check if the agent retried the same tool after denial (look at subsequent `tool_groups` in the same turn).
2. Check if the agent adapted (used a different tool or approach).
3. If the agent kept retrying without adaptation → **blind retry loop** (see 2D.4).

### 2E.3 Tool Error Cascade

Build a timeline of `tool_error` failures:

```
Turn seq | Tool | Error preview | Next action
---------|------|---------------|------------
```

Look for:
- Same tool failing repeatedly → tool is broken or args are wrong
- Error → rewrite from scratch → same error → **rewrite loop** (agent can't verify)
- Error → compaction → same error → **compaction-induced amnesia**

### 2E.4 Verification Gap Detection

A **verification gap** occurs when the agent cannot confirm its output works:
- Safety guards block all verification commands (grep, node -e, playwright)
- No browser automation available for HTML/JS output
- Agent declares success based on file existence, not functional testing

Signs in the digest:
- `safety_guard_blocks > 0` AND `tool_calls_failed / total_tool_calls > 0.15`
- Multiple turns with same `user_input_preview` pattern ("不行", "还是不行", "打开没东西")
- `tokens_in` growing monotonically without compaction (agent rewrites instead of fixing)

---

## Phase 3: Optional deep dive (only if user needs message-level proof)

- **Heavy checkpoints**: `~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json` — OpenAI-style messages the model saw.
- **Debug export**: If TARGET is a debug JSON file, read `schema` / `journal_turn_summary` when present.

Do not rebuild Phase-2 statistics from these files if digest already covered them.

### 3.1 Checkpoint Forensics (for debug focus)

When heavy checkpoints exist and FOCUS=debug:
- **System message size**: token count of system prompt
- **Tool schemas present**: count tool definitions
- **History depth**: assistant/user/tool turns in context
- **Largest tool result**: which tool result consumes most tokens
- **Repeated content**: same file path appearing multiple times
- **Nudge messages**: system-role messages containing "STALL" or "DIVERGENCE" or "avoid" — check if agent acknowledged them

---

## Phase 4: Report

### Standard Report (FOCUS ≠ debug)

Keep the report compact and grounded in digest JSON.

1. **Session**: `session_id` from digest; note `journal_file` path.
2. **Headline metrics**: copy key fields from `aggregates`; flag `safety_guard_blocks > 0`.
3. **Notable turns**: at most 3 entries, cite `seq` and `turn_id`.
4. **Issues**: bullet list tied to digest evidence (stalls, errors, token spikes, missing compactions, safety guard false positives from `failed_tool_calls`).
5. **Recommendations**: 3–5 actionable items.

### Debug Report (FOCUS=debug)

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
║  {secondary issues}                                          ║
║                                                              ║
║  💊 Recommended Fix                                          ║
║  {specific actions — code/config changes with file refs}     ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

### Common Debug Diagnoses & Fixes

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
| Journal digest CLI | `rust/crates/astra-cli/src/cli/journal_digest.rs` |
| Session journal | `rust/crates/services/src/session_journal.rs` |
| Tool selection | `rust/crates/runtime/src/turn/tool_selection.rs` |
| Compaction | `rust/crates/runtime/src/turn/cloud/compaction.rs` |
| Stall detection | `rust/crates/runtime/src/turn/stall.rs` |
| Turn guard & verdicts | `rust/crates/runtime/src/turn/turn_guard.rs` |
| Error recovery & escalation | `rust/crates/runtime/src/turn/error_recovery.rs` |
| Tool health tracking | `rust/crates/runtime/src/turn/tool_health.rs` |
| REPL turn handler | `rust/crates/astra-cli/src/cli/repl_turn.rs` |
| Debug command | `rust/crates/astra-cli/src/cli/slash_debug.rs` |
| Chat stream | `rust/crates/astra-cli/src/cli/chat_stream/` |

---

## Machine-Readable Output (auto-invoke)

When the skill is **auto-invoked** by the runtime's [`AutoInvokeGate`](../../rust/crates/astra-skills/src/auto_invoke.rs) (caller passes `--auto-invoke` or the runtime is wiring your output back into the next turn's SelfModel), append a fenced JSON block at the end of your response matching the `SkillDiagnosis` schema:

````markdown
```skill-diagnosis
{
  "schema_version": 2,
  "skill": "analyze_session",
  "cause": "session_stalls",
  "headline": "agent looping on grep in src/",
  "findings": [
    "grep invoked 4× with identical args (turns 5-8)",
    "no new matches since turn 3"
  ],
  "recommended_action": "switch to rg or narrow scope to src/",
  "success_criteria": [
    {
      "metric": "session_stalls_delta",
      "operator": "lte",
      "threshold": 0.0,
      "window_turns": 3,
      "description": "session stalls stop increasing after the recommendation"
    }
  ],
  "source": "real_skill"
}
```
````

**Contract (enforced by `SkillDiagnosis::parse_from_skill_output`):**

- `schema_version` must be `2`. Unknown versions are dropped.
- `cause` must be one of `session_stalls` | `budget_pressure` | `repeated_corrections`.
- `skill` should match `analyze_session` (the invoking skill's name).
- `headline` is one sentence, ≤160 chars.
- `findings` is a list of ≤5 short bullets, each ≤160 chars.
- `recommended_action` is optional; ≤160 chars.
- `success_criteria` is required and non-empty; each criterion must use a known metric/operator, finite numeric threshold, positive `window_turns`, and concise description.
- `source` must be `real_skill` for actual skill output. The runtime reserves `synthetic_fallback` for canned non-LLM diagnoses.
- Overflow is truncated silently by the parser — prefer concise output.
- If multiple blocks appear, **the last one wins**.

Keep the human-readable analysis above the block for the interactive user; the block is strictly for the runtime self-model.

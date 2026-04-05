# debug-session

Diagnose why an **astra** session is stuck, slow, looping, or producing bad results.
Deeper than `analyze-session` — focuses on root cause diagnosis by mapping symptoms
to astra's internal correction mechanisms.

## Usage

```
/skill debug-session
/skill debug-session --symptom stuck
/skill debug-session /tmp/debug-abc123-turn1.json --symptom slow
/skill debug-session last --symptom errors
```

## What It Diagnoses

| Symptom | What It Checks |
|---------|---------------|
| **stuck** | Stall patterns (sig_stall/name_stall/divergence), nudge effectiveness, escalation trajectory |
| **slow** | TTFT latency, context_ms bottleneck, selector_ms overhead, tool execution times |
| **looping** | Tool signature repetition, stall detector window coverage, blind retry patterns |
| **wrong-tools** | Tool health degradation, deprioritization, selector strategy mismatches |
| **errors** | Error cascades, compaction-induced amnesia, TurnGuard verdict history |

## Output

A structured diagnostic report with:
- **Root cause** — one-line diagnosis
- **Evidence chain** — timeline of symptom → escalation → correction
- **Internal state** — escalation level, nudge count, deprioritized tools
- **Recommended fix** — specific code/config changes with file references

## Key Concepts

- **Stall types**: sig_stall (same tool signature ≥3x), name_stall (same tool names), divergence (exploration-only)
- **Escalation levels**: Normal → Warning → Critical (based on nudge count + error count)
- **Correction records**: TurnGuard interventions with avoid_tools and alternatives
- **Tool health**: Per-tool success/failure tracking with deprioritization and rehabilitation

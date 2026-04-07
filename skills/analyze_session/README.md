# analyze-session

Deep diagnostic analysis and debugging of **astra** agent sessions. Reads the session journal (JSONL),
heavy checkpoints, and debug snapshots to identify inefficiencies in context management,
tool selection, token usage, error handling, compaction, and stall recovery.

> Now includes all debugging capabilities previously in `debug-session` — stall pattern
> analysis, turn guard escalation tracing, error cascade detection, tool health degradation,
> and correction effectiveness evaluation. Use `--focus debug` to access these.

## Usage

```
/skill analyze-session
/skill analyze-session /tmp/debug-abc123-turn1.json
/skill analyze-session --focus tokens
/skill analyze-session last --focus errors
/skill analyze-session --focus debug
/skill analyze-session --focus debug --symptom stuck
/skill analyze-session last --focus debug --symptom slow
```

## What It Analyzes

| Dimension | What It Checks |
|-----------|---------------|
| **Context** | Token growth curve, compaction events, budget pressure tiers, stale reasoning, tool schema bloat |
| **Tools** | Selection accuracy (tools_selected vs tools_used), selector strategy (tfidf/llm), missed parallelism, bash misuse |
| **Tokens** | Per-turn prompt/completion, TTFT, context build time, selector overhead |
| **Errors** | Error cascades, stall detection (sig_stall/name_stall/divergence), TurnGuardVerdict events, tool failure rates |
| **Flow** | Plan subtask tracking, delegation fan-out/aggregate, turn productivity classification |
| **Debug** | Root cause diagnosis: stall patterns, turn guard escalation, error cascades, tool health degradation, correction effectiveness, latency bottlenecks |

## Data Sources

| Source | Location | Format |
|--------|----------|--------|
| Session journal | `~/.astra/sessions/<id>.jsonl` | JSONL — one `JournalEvent` per line |
| Heavy checkpoints | `~/.astra/sessions/<id>/step_checkpoints/*-heavy.json` | JSON array of OpenAI messages |
| Debug snapshots | `/tmp/debug-<short>-turn<N>.json` | `astra-debug-turn-delta-v1` or `astra-debug-turn-full-v1` |
| Cloud events | `agent_events` table | Batch INSERTed via EventIngestionWorker |

## Output

### Standard Analysis (focus ≠ debug)
A structured health report with:
- **Health Score** (0–100) across 4 dimensions (context, tools, tokens, error handling)
- **Critical Issues** with astra-specific root causes
- **Warnings** for suboptimal patterns
- **Recommendations** with references to astra source files

### Debug Diagnosis (focus = debug)
A structured diagnostic report with:
- **Root cause** — one-line diagnosis
- **Evidence chain** — timeline of symptom → escalation → correction
- **Internal state** — escalation level, nudge count, deprioritized tools
- **Correction effectiveness** — which TurnGuard interventions worked
- **Recommended fix** — specific code/config changes with file references

## Astra-Specific Anti-Patterns Detected

- 📛 **No compaction**: budget_pressure=0 but tokens >80k
- 📛 **Aggressive compaction loop**: budget_pressure ≥0.9 for 3+ consecutive turns
- 📛 **Stale reasoning**: old reasoning_content surviving (strip_stale_reasoning failure)
- 📛 **Tool schema bloat**: >30 tools in tools_selected (each ~500 tokens)
- 📛 **Selector miss**: LLM tried tool not in tools_selected
- 📛 **Strategy mismatch**: tfidf for novel task where LLM selection needed
- 📛 **Skill injection waste**: skill injected but never referenced in output
- 📛 **Stall ignored**: StallDetected event followed by same tool pattern
- 📛 **Slow context build**: context_ms >2000ms
- 📛 **Output explosion**: single tool result >10KB inflating context
- 📛 **Blind retry**: Error → Retry → Same Error with no adaptation
- 📛 **Compaction amnesia**: Error → Compaction → Lost Context → More Errors
- 📛 **Guard stuck**: Warning level for 5+ turns without escalating to Critical

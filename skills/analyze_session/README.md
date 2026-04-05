# analyze-session

Deep diagnostic analysis of coding agent sessions — context quality, tool/skill/MCP selection,
token efficiency, error patterns, and execution flow.

## Usage

```
/skill analyze-session
/skill analyze-session /tmp/debug-abc123-turn1.json
/skill analyze-session --focus tokens
/skill analyze-session last --focus errors
```

## What It Analyzes

| Dimension | What It Checks |
|-----------|---------------|
| **Context** | System prompt bloat, history explosion, repeated file reads, stale context |
| **Tools** | Wrong tool selection, missed parallelism, redundant calls, MCP effectiveness |
| **Tokens** | Per-turn budget, waste indicators, cost estimate, thinking token ratio |
| **Errors** | Error cascades, recovery quality, blind retries, unhandled failures |
| **Flow** | Task decomposition quality, turn efficiency, decision quality |

## Output

A structured health report with:
- **Health Score** (0–100) across 4 dimensions
- **Critical Issues** that need immediate attention
- **Warnings** for suboptimal patterns
- **Recommendations** with specific, actionable fixes

## Data Sources

- JSON debug log files (`/tmp/debug-*.json`)
- Session events from the platform database (`agent_events` table)
- Current session context

## Anti-Patterns Detected

- 📛 History explosion (context grows >50% per turn)
- 📛 Shell-for-everything (using `bash` when specialized tools exist)
- 📛 Sequential reads (independent file reads not parallelized)
- 📛 Blind retry (same tool call repeated without changes)
- 📛 Premature coding (writing code before understanding the problem)
- 📛 No verification (changes without running tests/lint)
- 📛 Verbose thinking (>1000 thinking tokens for trivial decisions)

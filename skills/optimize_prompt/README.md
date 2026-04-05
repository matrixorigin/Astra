# optimize-prompt

Analyze and optimize everything **astra** sends to the LLM — system prompt, tool schemas,
conversation history, skill injections, and memory signals.

## Usage

```
/skill optimize-prompt
/skill optimize-prompt --component tools
/skill optimize-prompt /tmp/debug-abc123-turn1-full.json --component history
```

## What It Analyzes

| Component | Analysis |
|-----------|---------|
| **System** | Base identity, core rules, tool-conditional guidance, project profile, total size |
| **Tools** | Pinned vs dynamic schemas, selection accuracy, per-tool token cost, schema bloat |
| **History** | Tool result sizes, repeated content, stale reasoning, message type breakdown |
| **Skills** | Injected skills, relevance to task, token overhead |
| **Budget** | Pressure timeline, compaction tier thresholds (0.3/0.6/0.9), model utilization |

## Key Metrics

- **Budget pressure tiers**: Normal (<60%), TrimSchemas (60-75%), CompactHistory (75-85%), AggressivePrune (>85%)
- **Tool selection accuracy**: `|tools_used| / |tools_selected|` — target >70%
- **System prompt baseline**: ~14K tokens (default estimate)
- **Token estimation**: CJK-aware (1.5 tokens/char), JSON-aware (2 bytes/token)

## Output

Token budget breakdown per component with savings opportunities (estimated tokens recoverable).

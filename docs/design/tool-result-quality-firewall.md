# Tool result quality firewall

> Status: target design contract.
> Last updated: 2026-07-07.

The tool result quality firewall evaluates tool outputs before they become model context or durable learning signals. It is a quality and safety layer, not a replacement for tool execution correctness.

## Goals

- Detect malformed, incomplete, unsafe, or misleading tool results.
- Annotate uncertainty before results enter the next model round.
- Prevent low-quality results from silently becoming facts.
- Provide structured retry, fallback, or user-facing diagnostics.
- Feed evaluation and tuning with high-quality labels.

## Non-goals

- Do not hide tool failures as normal results.
- Do not rewrite tool output without preserving provenance.
- Do not turn heuristic quality scores into hard truth.
- Do not use raw sensitive tool output for learning without consent and redaction.

## Quality dimensions

| Dimension | Examples |
| --- | --- |
| Completeness | expected fields missing, truncated output, partial timeout. |
| Validity | invalid JSON, schema mismatch, binary/control bytes in text channel. |
| Relevance | result does not answer requested query or path. |
| Safety | secrets, unsafe terminal control, suspicious binary data. |
| Freshness | stale cache, old sync watermark, outdated provider state. |
| Confidence | tool succeeded but semantic confidence is low. |

## Result envelope

Tool results should be normalized into an envelope:

```text
tool_call_id
tool_name
provider_id
status
raw_result_ref
visible_summary
quality_status
quality_reasons
redaction_status
retry_hint
fallback_hint
trace_event_id
```

The model should see the visible summary and quality annotation, not arbitrary unclassified bytes.

## Actions

| Quality status | Action |
| --- | --- |
| good | Pass to model and trace. |
| degraded | Pass with warning and retry/fallback hint. |
| incomplete | Ask model to retry or continue with caveat. |
| unsafe | Block visible content and report policy reason. |
| malformed | Isolate, trace, and avoid poisoning context. |
| too_large | Summarize, chunk, or store as artifact with manifest. |

## Integration points

- Capability system: provider/result status and fallback decisions.
- Context and prompt: quality annotations in dynamic context.
- Observation plane: C3 trace facts for quality decisions.
- Safety: redaction and unsafe output handling.
- Evaluation and tuning: labels for tool reliability and prompt/tool improvements.

## Test obligations

- Malformed output does not crash stream handling.
- Unsafe terminal control bytes are stripped or blocked before UI rendering.
- Partial timeout output is surfaced as degraded, not success.
- Large output becomes artifact/summary with manifest.
- Quality annotations survive compaction and replay.

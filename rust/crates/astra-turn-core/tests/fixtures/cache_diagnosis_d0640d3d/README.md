# cache_diagnosis_d0640d3d — regression fixture

Scrubbed copy of 19 `llm_capture_*.json` files from live session
`d0640d3d-3be0-4ce1-a4b7-e52d49601da6` (2026-05-08). This session
exposed every cache-regression the introspect `subtopic=cache`
diagnostic is designed to catch, so it serves as the single-source
regression net — one fixture, all rules triggered.

## What's in each file

- `t3_r0.json` — first request, provider=anthropic, model=deepseek-v4-pro-anthropic.
  Shows the **tool-marker-not-on-tail** pathology: 21 tools served,
  `cache_control` landed on `skill` (idx 19), `web_search` (idx 20) fell
  out of cache. Observed `cache_read=2432` (29% hit) out of 8529.

- `t4_r0.json`, `t4_r1.json`, `t5_r0.json`, `t5_r1.json`, `t6_r0.json..t6_r13.json` —
  provider=bedrock, model=us.anthropic.claude-opus-4-7. The t6 series is the
  14-round agentic tool loop that surfaced **cc-marker-frozen** and
  **cache-creation-waste** pathologies: cc indices `[0, 8, 10]` stayed put
  across all 14 rounds while `(assistant_tool_call, tool_result)` pairs
  appended at msg[12..=39], so `cache_read` flatlined at 11312 and
  `cache_creation` summed to ~44K wasted tokens (94% of total creation).

## Scrubbing

All free-form text payloads (`content`, `thinking`, `reasoning_content`,
tool arguments, tool schema bodies) are replaced with
`<{sha256_prefix}:{len}>` digests. This keeps byte-level change detection
(so rules that compare bytes across rounds still work) while stripping user
queries, file contents, and model completions. Structural fields relevant
to cache diagnosis — `cache_control`, role, index, tool count, `usage`,
`tool_calls[].id/name`, `tool_call_id` — are preserved verbatim.

`session_id` is replaced with the literal `"d0640d3d-SCRUBBED"`.

## Do NOT modify by hand

This fixture is the source of truth for `cache_diagnosis` regression tests.
If the production capture format changes, regenerate the whole directory
from a fresh session rather than patching individual files — the rules
care about cross-round invariants, and piecemeal edits can silently
desync the set.

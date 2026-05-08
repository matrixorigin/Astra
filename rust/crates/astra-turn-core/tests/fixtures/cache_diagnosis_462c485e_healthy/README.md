# cache_diagnosis_462c485e_healthy — clean Bedrock multi-round fixture

5 scrubbed `llm_capture_*.json` files from live session
`462c485e-084f-4f3d-82d4-2e2e1b657d6d` (2026-05-08). This session
captured a **healthy** multi-turn Bedrock run with every cache
invariant satisfied:

| round | cache_read | tool_cc_index | msg_cc_indices |
| ----- | ---------- | ------------- | -------------- |
| t1 r0 | 8426       | 20 (last)     | [0, 1]         |
| t1 r1 | 8426       | 20 (last)     | [0, 4]         |
| t1 r2 | 8426       | 20 (last)     | [0, 6]         |
| t1 r3 | 8426       | 20 (last)     | [0, 7, 9]      |
| t1 r4 | 8426       | 20 (last)     | [0, 9, 11]     |

Why this fixture matters: nothing here should trip `evaluate_all`.
It's the negative counterpart to `cache_diagnosis_d0640d3d` (where
every rule fires) and `cache_diagnosis_986a553e` (where the strict-
history rule fires). A future refactor that silently makes ANY rule
over-eager on a clearly healthy session will fail the regression
test — giving us an early warning before it ships.

## What this fixture preserves

Scrubber keeps the **first 200 chars past each volatile marker**
(`## Self-Awareness`, `[session-memory:`, `[attention:v1]`) so
`contains_volatile_pattern` still detects the pattern. The rest of
each message body is replaced with `<sha256prefix:length>` digests.
Structural fields the rules care about — `usage`, roles, `tool_count`,
message indices, `cache_control` markers — are preserved verbatim.

## Rule coverage

Loading this fixture through `evaluate_all` should trigger **no**
findings. Specifically silent:

- `cc_marker_frozen` — rolling msg_cc advances every round
- `tool_marker_not_on_tail` — tool_cc_index = 20 sits on the last tool
- `cache_read_collapsed` — cache_read is a flat 8426 across all 5 rounds
- `cache_creation_waste` — 5-round session, post-first-round
  creation/read ratio is well under the 0.3 threshold
- `volatile_in_cached_prefix` — volatile lives AFTER the last
  cache_control marker inside the system block (MarkerIsolated
  placement on Bedrock)

## Do NOT edit by hand

Regeneration script lives inline in the commit that introduced this
fixture. If `contains_volatile_pattern` gains new entries, regenerate
so the preserved-prefix cutoff includes the new markers.

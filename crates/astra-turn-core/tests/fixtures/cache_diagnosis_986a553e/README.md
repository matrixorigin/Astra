# cache_diagnosis_986a553e — MiniMax tool-loop regression fixture

9 scrubbed `llm_capture_*.json` files from live session
`986a553e-b0e5-4570-bcd2-a47a11c41a15` (2026-05-08). This session was
captured AFTER the d0640d3d rolling-breakpoint + deferred-tool surface fixes
landed and exposed a **new** class of regression: `MiniMax-M2.7`
(provider=openai) shows `cache_read` collapsing from 7680 tokens at
`t4 r0` to **zero** for six consecutive tool-loop rounds (`t4 r1..r6`).

## Root cause

Self-Awareness block (`## Self-Awareness\nTurn: N | Tokens: M/80000`)
is injected into a synthetic user-role "volatile preamble" message
that re-renders every round with the live turn and token counters.
MiniMax's OpenAI-compatible prompt cache uses **strict history
matching**: any byte change mid-history invalidates the entire cache
hit, unlike OpenAI's auto-prefix which would still match the stable
portion.

The fix lands in a new `cache_placement` module (see the `C1` commit
on `improve_promts`) that classifies MiniMax as `StrictHistoryMatch`
with `VolatilePlacement::CurrentUserOnly` — volatile content gets
injected only on round 0 of a visible turn, and skipped on tool-loop
continuations.

## What this fixture preserves

Scrubber keeps the **first 200 chars past each volatile marker**
(`## Self-Awareness`, `[session-memory:`, `[attention:v1]`) so
`contains_volatile_pattern` still detects the pattern. The rest of
each message body is replaced with `<sha256prefix:length>` digests.
Structural fields the rules care about — `usage`, roles, `tool_count`,
message indices — are preserved verbatim.

## Rule coverage

Loading this fixture through `evaluate_all` should trigger:

- `cache_read_collapsed` — cache_read=7680 at t4 r0 → 0 at t4 r1
- `volatile_in_cached_prefix` — MiniMax strict-history tool-loop
  round with volatile content at msg[7] on round >0

And specifically should NOT trigger:

- `cc_marker_frozen` — MiniMax has no cache_control markers
- `tool_marker_not_on_tail` — same reason (no tool cc index)
- `cache_creation_waste` — MiniMax doesn't report cache_creation

## Do NOT edit by hand

Regeneration script lives in commit history. If the
`contains_volatile_pattern` list gains new entries, regenerate so the
preserved-prefix cutoff includes the new markers.

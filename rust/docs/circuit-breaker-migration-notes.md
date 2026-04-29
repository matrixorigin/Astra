# Circuit Breaker — Configuration Changes (feat/thinking-mode)

This branch updates defaults and adds one new knob for the agentic-loop
circuit breaker in `astra-turn-core::loop_circuit_breaker`. All changes are
opt-out via explicit config; no runtime API changes.

## Defaults

| Knob (`ToolSelectionConfig`)                         | Old | New | Floor (old → new) |
|------------------------------------------------------|-----|-----|-------------------|
| `circuit_breaker_read_only_stall_threshold`          |  8  | 12  | 3 → 4             |
| `circuit_breaker_max_introspect_emissions` (new)     |  —  |  3  | —  → 1            |

`0` continues to mean "use the default value" for every knob.

## Migration

- Users who explicitly set `circuit_breaker_read_only_stall_threshold = 3`
  in `runtime.toml` will be silently floored to **4**. Non-breaking, but
  behavior shifts by one round.
- Users who relied on the old default of `8` (no explicit value) will now
  see the first introspect soft-signal at round **12** instead of **8**.
  Combined with progress-aware stall detection, genuinely exploratory
  turns (e.g. reviewing 12+ unique files) no longer false-trip.
- New `circuit_breaker_max_introspect_emissions` caps self-check prompts
  at **3** per turn by default. Set to a very large value (e.g.
  `u32::MAX`) for effectively unbounded behavior; `0` is reserved for
  "use default". The counter resets on any mutating tool call.

## Related tests

- `astra-turn-core::loop_circuit_breaker::tests::default_read_only_stall_threshold_is_12`
- `..::default_max_introspect_emissions_is_3`
- `..::introspect_emissions_are_capped`
- `..::introspect_cap_resets_on_mutation`
- `..::introspect_cap_zero_is_unbounded`
- `..::code_review_12_unique_reads_never_trips`
- `astra-cli::cli::chat_stream::sse_loop::tests::circuit_breaker_config_uses_runtime_config_defaults`
- `..::circuit_breaker_config_uses_runtime_config_overrides_with_floors`

---

# Loop Convergence Hardening (session-36500dd9 follow-up)

Investigation of session `36500dd9-89bc-4da8-b677-dab7c7702116` (turn 4:
37-token conceptual question "为啥其他models看不到thinking" → 23 tool
calls across 12 rounds → circuit-breaker abort) identified four
compounding failures. This section documents the targeted fixes.

## 1. System prompt — Turn Discipline section

`runtime::prompts::system::turn_discipline_section()` adds five rules:
announce-once-briefly, end-of-turn summary, no externalized reasoning,
lead-with-the-answer, match depth to task. These are soft convergence
signals — empirically turns that churn >10 rounds lack a standing
commitment to summarize, and requiring the summary creates self-check
pressure ("have I gathered enough yet?") at each round.

Prompt-size budget tests in `prompts::mod.rs` bumped 12500→13000 and
19000→19800 to accommodate.

## 2. QuickAnswer scenario

New variant `astra_config::user_profile::Scenario::QuickAnswer` with the
tightest strategy in the set (`max_tools_per_turn=5`,
`tool_budget_tokens=500`, `memory_top_k=5`).

`runtime::turn::agentic_adaptive_tuning::fallback_scenario_from_routing`
routes short interrogative read-only queries here BEFORE Exploration.
Preconditions:

- query ≤ 200 chars
- starts/ends with interrogative marker (word-boundary-matched for
  English: why/what/where/which/how/who/whose/whom; substring-matched
  for Chinese: 为啥/为什么/怎么/哪里/哪个/什么是/什么情况) or ends `?`/`？`
- read-only (`!task_profile.mutates_workspace`)
- review/debug keywords do NOT take precedence (those scenarios imply
  deeper intent even on short queries)

`Exploration`'s config is intentionally NOT tightened — it remains
appropriate for real exploration. The fix is routing, not action.

## 3. Circuit breaker — physical tool lockout + clearer corrective

Previously `round_budget_phase1_message` said "tools disabled" but the
runtime did NOT actually restrict the next round's tool list. The model
sometimes ignored the message and kept calling tools; phase 2 then
aborted. Two changes:

- Corrective wording is now declarative: "Any tool calls you emit WILL
  BE DROPPED before execution" — matches shapes the model was trained
  to respect.
- `agentic_loop_execution_phase` injection site now inserts every
  `host.valid_tool_names()` into `state.restricted_tools`, so the next
  round's tool-selection payload is empty. Restricted_tools is cleared
  in `agentic_loop_finalization` at turn boundaries (already tested),
  so the lockout scope is one round only and has zero effect on normal
  flow.

## 4. Dynamic thinking budget scaling

`astra_turn_core::thinking_config::ThinkingConfig::scale_for_turn` treats
the user's `/model` pick (e.g. `thinking:high`) as a CEILING, not a
floor. For lightweight turns (≤120 chars, read-only, no continuation /
modification intent) it caps `Adaptive{High/Max}` to `Medium` and caps
`Enabled{budget}` at 4000 tokens. Never increases effort; never turns
thinking off.

Invoked once per turn in
`astra-cli::cli::chat_stream::sse_loop::agentic_loop_turn::prepare_chat_turn_payload`.
The user's stored preference is unchanged — scaling is per-turn only.

## Related tests

- `astra_turn_core::thinking_config::tests::scale_for_turn_*` (8 tests)
- `astra_runtime::turn::agentic_adaptive_tuning::tests::short_interrogative_routes_to_quick_answer_not_exploration`
- `..::short_chinese_interrogative_routes_to_quick_answer`
- `..::debug_keyword_wins_over_quick_answer`
- `..::long_question_does_not_route_to_quick_answer`
- `..::non_interrogative_short_query_does_not_route_to_quick_answer`

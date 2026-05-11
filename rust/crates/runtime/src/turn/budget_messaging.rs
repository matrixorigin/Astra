//! Volatile-lane message templates for high-context-pressure events.
//!
//! Two templates ship here as constants so the exact wire text is
//! reviewable in one place and easy to keep factual (as opposed to
//! imperative / adversarial). Past versions issued "Do NOT call any more
//! tools. Summarize your progress." which reliably turned mid-task agents
//! into read-only summarizers — the exact conservative-stop failure mode
//! users reported under high context usage.
//!
//! Design notes (why factual, not imperative):
//!   * astra targets many models, not just Claude. A strong model treats a
//!     soft fact statement ("Context 185k/200k, compact freed 18k") as a
//!     checkpoint and continues. A weak model in the same spot may need
//!     the extra nudge — but a HARD ban on tools ("Do NOT call any more
//!     tools") disables the only actions that would complete the task.
//!   * Both messages ride the same volatile lane on the same turn when
//!     compaction succeeds. If both were imperative they would stack into
//!     a contradictory prompt ("continue"/"do not call tools"). Keeping
//!     them factual makes stacking safe.
//!   * "continue" + factual state is enough for a model to proceed on a
//!     task that still fits post-compact. If it doesn't fit post-compact,
//!     the compaction pipeline already short-circuits via the circuit
//!     breaker added in `compaction_replay`.

/// Advisory posted when measured input tokens still exceed
/// `max_turn_input_tokens` AFTER the aggressive compression pipeline and
/// spill-to-disk have both run. The agent is informed but not disabled;
/// it remains free to call tools or produce output as the task requires.
///
/// Format is intentionally runtime-agnostic: no references to ✨ compact
/// events, no claims about what the agent "should" do. Just the fact that
/// the runtime observed pressure.
pub const BUDGET_REACHED_ADVISORY: &str = "Context note: input tokens have reached the configured per-turn \
     budget. The runtime has already attempted compaction and spill; \
     further token savings this turn are unlikely. Continue the task — \
     prefer concise tool calls and avoid re-reading material that is \
     already in context. If you must stop, do so because the task is \
     complete or genuinely blocked, not because of this notice.";

/// Directive posted on the SAME turn when the aggressive compression
/// pipeline has just shortened the visible history. The model often
/// misreads a sudden shorter context as "I was interrupted" and produces
/// a progress summary instead of continuing. This template reframes the
/// event as transparent runtime housekeeping.
///
/// Keep it factual: state what happened ("history was compressed") and
/// what the expected action is ("continue"). Do NOT layer a prohibition
/// ("do not summarize") on top — if the agent naturally needs to
/// summarize, that's the correct action; the problem is a panic-summary,
/// not all summaries.
pub const COMPACT_RESUME_DIRECTIVE: &str = "Context compacted: the runtime just compressed older conversation \
     history to reduce token pressure. Your original task and the most \
     recent tool activity are still present above. Continue the task — \
     keep working where you left off; treat this compaction as a \
     transparent runtime event.";

//! Volatile-lane message templates for high-context-pressure events.
//!
//! Pure-fact contract (first principle): a runtime event notification
//! states WHAT happened and identifies the authoritative user request. It
//! must NOT introduce a behavioral fork ("Continue only if X, otherwise
//! Y") because weak models under budget pressure resolve the fork toward
//! early termination, and must NOT issue imperatives ("Avoid ...", "Do
//! NOT ...") because that constrains the model's action space. The
//! latest user request remains authoritative; the runtime only reports
//! state.
//!
//! Stacking safety: both messages ride the same volatile lane on the same
//! turn when compaction succeeds. Because each is a pure fact, stacking
//! cannot produce contradictory directives. If the task no longer fits
//! post-compact, the compaction pipeline short-circuits via the circuit
//! breaker in `compaction_replay`.

/// Advisory posted when measured input tokens still exceed
/// `max_turn_input_tokens` AFTER the aggressive compression pipeline and
/// spill-to-disk have both run. Pure fact: states the measured pressure
/// and the runtime actions already taken. The agent's action space is
/// unconstrained; the latest user request remains authoritative.
pub const BUDGET_REACHED_ADVISORY: &str = "Context note: input tokens reached the configured \
     current-turn budget for the latest user request. The runtime has \
     already attempted compaction and spill; further token savings in this \
     turn are unlikely. This notice is not a new user request; the latest \
     user request remains authoritative.";

/// Context note posted on the SAME turn when the aggressive compression
/// pipeline has just shortened the visible history. Pure fact: states
/// that compaction occurred and what remains present. The model's action
/// space is unconstrained; the latest user request remains authoritative.
pub const COMPACT_RESUME_DIRECTIVE: &str = "Context compacted: the runtime compressed older \
     conversation history to reduce token pressure. The original task and \
     the most recent tool activity are still present above. The turn continues \
     from the current state under the latest user request; compaction did not \
     create a new request for a progress summary. This notice is not a new user \
     request; the latest user request remains authoritative.";

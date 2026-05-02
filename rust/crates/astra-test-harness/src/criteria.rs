//! Criterion evaluation.
//!
//! Criteria stack from cheap-to-expensive. Deterministic matchers
//! (tool_called, stderr_contains, exit_code, tools_count) run
//! against the captured `RunOutcome` locally — no provider calls.
//! The `Judger` variant calls into an LLM to score free-form
//! criteria like "did the agent understand the task?".
//!
//! Rationale: when a deterministic criterion fails, the case is
//! already known FAIL and the judger call would waste a provider
//! round-trip. The runner short-circuits accordingly (see
//! `evaluate_all`).

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::runner::RunOutcome;
use crate::session_capture::SessionCapture;

/// One declarative success check. Serialized into YAML cases as
/// `type: <variant>` discriminator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Criterion {
    /// Passes if the tool with `name` appears in the run's
    /// `tools_used` list. Cheapest possible check.
    ToolCalled { name: String },

    /// Passes when the run's exit code equals `code`. Useful for
    /// pinning expected failures.
    ExitCode { code: i32 },

    /// Passes when the total tool_calls_count is within the range
    /// `min..=max`. Catches runaway loops or under-tool-use.
    ToolsCountBetween { min: u32, max: u32 },

    /// Regex match against the run's stderr. Intended for
    /// observability checks — `^\[fork-cache\]` / `^\[audit\]`.
    /// The regex is compiled per-evaluation; test stays
    /// robust across Rust regex version bumps.
    StderrMatches { pattern: String },

    /// Passes when the final assistant text contains `needle`
    /// (case-sensitive substring match). For simple yes/no
    /// checks without pulling in a judger.
    TextContains { needle: String },

    /// Passes when the session journal contains at least `min`
    /// events with `type == event_type`. Requires session capture.
    /// Use for structural checks ("at least one subagent_spawned
    /// event appears").
    ///
    /// FAILS when no session is loaded, UNLESS `optional: true` —
    /// in which case the criterion skip-passes with a note. Default
    /// strict semantics mean: if a case author wrote this criterion
    /// and the session isn't available, something is wrong (bad
    /// session_id, journal not flushed, loader misconfigured) and
    /// the case should surface that as a failure instead of silently
    /// passing.
    SessionEventCount {
        event_type: String,
        #[serde(default = "default_event_min")]
        min: u32,
        /// When true, skip-pass when session is unavailable. Use
        /// sparingly — only for cases that are meaningful even
        /// without the journal check.
        #[serde(default)]
        optional: bool,
    },

    /// Passes when the given tool name appears in the journal's
    /// `tool_invocation` events. Journal is the source of truth
    /// for tool calls — `tools_used` from the CLI envelope may
    /// miss tools emitted inside sub-agent runs.
    ///
    /// FAILS when no session is loaded unless `optional: true`. See
    /// `SessionEventCount` for the rationale.
    JournalToolCalled {
        name: String,
        #[serde(default)]
        optional: bool,
    },

    /// Passes when at least one `[fork-cache]` JSON event in stderr
    /// has its `outcome` field in `expect`. Pins the exact runtime
    /// contract (see `ForkCacheEvent` in astra-turn-core) — outcomes
    /// are one of `hit`, `partial_drift`, `miss`, `exceeded_expected`.
    ///
    /// Example:
    /// ```yaml
    /// - type: fork_cache_outcome
    ///   expect: [hit]
    /// ```
    ///
    /// Accepted field aliases: `outcome` (current wire name) and
    /// `class` (earlier harness-facing name; deprecated, still read so
    /// existing YAML doesn't silently break).
    ForkCacheOutcome {
        /// Accepted outcome names (snake_case — `hit`, `partial_drift`,
        /// `miss`, `exceeded_expected`, plus any future variant added
        /// to `astra_turn_core::fork_cache_event::ForkCacheOutcome`).
        /// A stderr event whose `outcome` equals any of these passes.
        #[serde(default)]
        expect: Vec<String>,
    },

    /// LLM judger — calls a scoring model with the prompt +
    /// context and expects a number in [0.0, 1.0]. Passes when
    /// score >= `threshold`. Most expensive; put last.
    Judger {
        /// Natural-language question the judger answers. Should be
        /// specific: "Did the agent correctly spawn a sub-agent
        /// and return the agent_id?" — not "Is the output good?"
        question: String,
        /// Score threshold in [0.0, 1.0]. Default 0.7 — tests
        /// that allow mild model drift while rejecting
        /// obvious failures.
        #[serde(default = "default_judger_threshold")]
        threshold: f64,
        /// Optional model override for the judger. Defaults to
        /// whatever the harness's `--judger-model` CLI flag says.
        #[serde(default)]
        model: Option<String>,
    },

    /// Passes when total tokens (prompt + completion) is within range.
    /// Catches token efficiency regressions — a case that used to cost
    /// 500 tokens suddenly costing 5000 means something broke.
    TokensBetween { min: u64, max: u64 },

    /// Passes when wall-clock duration (ms) is within range.
    /// Catches latency regressions and hung subprocesses that
    /// complete just under the timeout.
    DurationBetween { min_ms: u64, max_ms: u64 },

    /// Passes when the tools_used list contains the given names
    /// as an ordered subsequence. Does NOT require exact match —
    /// extra tools between the expected ones are allowed.
    /// Example: `[read_file, str_replace]` passes for
    /// `[bash, read_file, bash, str_replace, bash]`.
    ToolSequence { tools: Vec<String> },

    /// Passes when the number of LLM round-trips (turns) is within range.
    /// Catches inefficient multi-turn loops where the agent should have
    /// completed in fewer rounds.
    TurnRoundsBetween { min: u32, max: u32 },

    /// Passes when the tool cache hit rate >= threshold (0.0 to 1.0).
    /// A high cache rate means the agent is efficiently reusing
    /// idempotent tool results. Requires at least one tool call.
    CacheRateAbove {
        /// Minimum cache hit rate (0.0 = no caching required, 1.0 = all cached).
        threshold: f64,
        /// Minimum number of tool calls required for the criterion to
        /// apply. When the agent makes fewer calls than this, the
        /// criterion FAILs instead of skip-passing. Default 1 — set
        /// higher if the case expects a specific tool-call volume.
        #[serde(default = "default_cache_min_calls")]
        min_calls: u32,
    },
}

fn default_cache_min_calls() -> u32 {
    1
}

fn default_judger_threshold() -> f64 {
    0.7
}

fn default_event_min() -> u32 {
    1
}

/// How severe a criterion failure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionSeverity {
    /// Hard requirement: exit_code, tool_called, text_contains.
    /// Failure means the case fundamentally didn't work.
    Hard,
    /// Soft bound: tokens_between, duration_between, turn_rounds, cache_rate.
    /// Failure means the case worked but outside acceptable efficiency bounds.
    Soft,
    /// Quality score: judger, session checks.
    /// Uses a 0-1 continuous score rather than binary pass/fail.
    Quality,
}

/// Result of evaluating a single criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion: Criterion,
    pub passed: bool,
    /// Severity level — tells the frontend how to treat this result.
    #[serde(default = "default_severity")]
    pub severity: CriterionSeverity,
    /// Short human explanation (≤ 200 chars). Surfaces in report
    /// on FAIL; suppressed on PASS unless `--verbose`.
    pub detail: String,
    /// Optional untruncated diagnostic. The Judger path fills this
    /// with the full judge text (including all quorum votes) so a
    /// FAIL report can show everything without re-running. `None`
    /// when the short `detail` already contains the full story.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub full_detail: Option<String>,
    /// For Judger only: the score the judger returned.
    #[serde(default)]
    pub score: Option<f64>,
}

fn default_severity() -> CriterionSeverity {
    CriterionSeverity::Hard
}

/// Classify the severity of a criterion type.
pub fn criterion_severity(c: &Criterion) -> CriterionSeverity {
    match c {
        Criterion::ExitCode { .. }
        | Criterion::ToolCalled { .. }
        | Criterion::TextContains { .. }
        | Criterion::ToolSequence { .. }
        | Criterion::ForkCacheOutcome { .. } => CriterionSeverity::Hard,

        Criterion::ToolsCountBetween { .. }
        | Criterion::TokensBetween { .. }
        | Criterion::DurationBetween { .. }
        | Criterion::TurnRoundsBetween { .. }
        | Criterion::CacheRateAbove { .. }
        | Criterion::StderrMatches { .. } => CriterionSeverity::Soft,

        Criterion::Judger { .. }
        | Criterion::SessionEventCount { .. }
        | Criterion::JournalToolCalled { .. } => CriterionSeverity::Quality,
    }
}

/// Evaluate every criterion against the outcome in list order.
/// Returns all results (not just first failure) so the report can
/// show which specific checks passed.
///
/// Session-dependent criteria (`SessionEventCount`, `JournalToolCalled`)
/// require the loaded session; pass `None` when session capture is off
/// and they'll auto-PASS with a clear "skipped" detail line.
///
/// For Judger criteria the runner calls into
/// [`crate::judger::Judger`] separately — this function is the
/// deterministic-only pass; see [`crate::suite::SuiteRunner`] for
/// the full orchestration.
pub fn evaluate_deterministic(
    criteria: &[Criterion],
    outcome: &RunOutcome,
) -> Vec<CriterionResult> {
    evaluate_deterministic_with_session(criteria, outcome, None)
}

/// Session-aware variant of [`evaluate_deterministic`]. The runner
/// calls this after loading the journal (if any).
pub fn evaluate_deterministic_with_session(
    criteria: &[Criterion],
    outcome: &RunOutcome,
    session: Option<&SessionCapture>,
) -> Vec<CriterionResult> {
    criteria
        .iter()
        .map(|c| evaluate_one(c, outcome, session))
        .collect()
}

fn evaluate_one(
    c: &Criterion,
    outcome: &RunOutcome,
    session: Option<&SessionCapture>,
) -> CriterionResult {
    match c {
        Criterion::ToolCalled { name } => {
            let hit = outcome.tools_used.iter().any(|t| t == name);
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: hit,
                detail: if hit {
                    format!("tool {name} was called")
                } else {
                    format!(
                        "tool {name} NOT called (tools_used: {:?})",
                        outcome.tools_used
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::ExitCode { code } => {
            let pass = outcome.exit_code == *code;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("exit_code {} (expected {})", outcome.exit_code, code),
                full_detail: None,
                score: None,
            }
        }
        Criterion::ToolsCountBetween { min, max } => {
            let n = outcome.tool_calls_count;
            let pass = n >= *min && n <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("tool_calls_count={n}, expected {min}..={max}"),
                full_detail: None,
                score: None,
            }
        }
        Criterion::StderrMatches { pattern } => {
            // Multi-line mode so `^` / `$` anchor at line boundaries —
            // stderr is almost always a log stream, and users write
            // patterns like `^\[fork-cache\]` expecting per-line match.
            match Regex::new(&format!("(?m){pattern}")) {
                Ok(re) => {
                    let hit = re.is_match(&outcome.stderr);
                    CriterionResult {
                        criterion: c.clone(),
                        severity: criterion_severity(c),
                        passed: hit,
                        detail: if hit {
                            format!("stderr matches /{pattern}/")
                        } else {
                            format!(
                                "stderr does NOT match /{pattern}/ (stderr len={})",
                                outcome.stderr.len()
                            )
                        },
                        full_detail: None,
                        score: None,
                    }
                }
                Err(e) => CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: format!("invalid regex /{pattern}/: {e}"),
                    full_detail: None,
                    score: None,
                },
            }
        }
        Criterion::TextContains { needle } => {
            let hit = outcome.text.contains(needle);
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: hit,
                detail: if hit {
                    format!("text contains {needle:?}")
                } else {
                    format!(
                        "text does NOT contain {needle:?} (text len={})",
                        outcome.text.len()
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::SessionEventCount {
            event_type,
            min,
            optional,
        } => {
            let Some(sess) = session else {
                // Session unavailable. Default (strict): FAIL with
                // an actionable detail so the reviewer knows the
                // criterion was requested but couldn't run. Setting
                // `optional: true` opts into skip-pass.
                let passed = *optional;
                let detail = if *optional {
                    format!(
                        "session_event_count {event_type} skipped (optional + no session capture)"
                    )
                } else {
                    format!(
                        "session_event_count {event_type} FAILED: no session \
                         loaded (enable debug_log: true in the case or \
                         --capture-session on the CLI; set optional: true on \
                         the criterion to skip-pass instead)"
                    )
                };
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed,
                    detail,
                    full_detail: None,
                    score: if passed { Some(1.0) } else { Some(0.0) },
                };
            };
            let n = sess.count_events(event_type);
            let pass = n as u32 >= *min;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("session events type={event_type} count={n} (expected >= {min})"),
                full_detail: None,
                score: if pass { Some(1.0) } else { Some(0.0) },
            }
        }
        Criterion::JournalToolCalled { name, optional } => {
            let Some(sess) = session else {
                let passed = *optional;
                let detail = if *optional {
                    format!("journal_tool_called {name} skipped (optional + no session capture)")
                } else {
                    format!(
                        "journal_tool_called {name} FAILED: no session loaded \
                         (enable debug_log: true / --capture-session; or set \
                         optional: true on the criterion to skip-pass)"
                    )
                };
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed,
                    detail,
                    full_detail: None,
                    score: if passed { Some(1.0) } else { Some(0.0) },
                };
            };
            let tools = sess.tools_invoked();
            let hit = tools.iter().any(|t| t == name);
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: hit,
                detail: if hit {
                    format!("journal tool {name} was invoked")
                } else {
                    format!("journal tool {name} NOT invoked (journal tools: {tools:?})")
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::ForkCacheOutcome { expect } => {
            let hits = parse_fork_cache_outcomes(&outcome.stderr);
            let pass = hits.iter().any(|c| expect.iter().any(|e| e == c));
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: if pass {
                    format!(
                        "fork-cache event with outcome in {expect:?} observed (all seen: {hits:?})"
                    )
                } else if hits.is_empty() {
                    "no [fork-cache] events observed in stderr".to_string()
                } else {
                    format!("no [fork-cache] event matched {expect:?}; seen outcomes: {hits:?}")
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::Judger { .. } => CriterionResult {
            criterion: c.clone(),
            severity: criterion_severity(c),
            passed: false,
            detail: "judger not yet evaluated (handled by runner)".into(),
            full_detail: None,
            score: None,
        },

        Criterion::TokensBetween { min, max } => {
            let total = outcome
                .prompt_tokens
                .saturating_add(outcome.completion_tokens);
            let passed = total >= *min && total <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!("tokens_total={total}, expected {min}..={max}"),
                full_detail: None,
                score: None,
            }
        }

        Criterion::DurationBetween { min_ms, max_ms } => {
            let dur = outcome.duration_ms;
            let passed = dur >= *min_ms && dur <= *max_ms;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!("duration={dur}ms, expected {min_ms}..={max_ms}ms"),
                full_detail: None,
                score: None,
            }
        }

        Criterion::ToolSequence { tools } => {
            // Check if `tools` is an ordered subsequence of `outcome.tools_used`.
            let mut iter = outcome.tools_used.iter();
            let mut matched = 0;
            for expected in tools {
                if iter.any(|t| t == expected) {
                    matched += 1;
                }
            }
            let passed = matched == tools.len();
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if passed {
                    format!("tool sequence {:?} found", tools)
                } else {
                    format!(
                        "tool sequence {:?} NOT found (matched {}/{}, actual: {:?})",
                        tools,
                        matched,
                        tools.len(),
                        outcome.tools_used
                    )
                },
                full_detail: None,
                score: None,
            }
        }

        Criterion::TurnRoundsBetween { min, max } => {
            let rounds = outcome.turn_rounds;
            let passed = rounds >= *min && rounds <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!("turn_rounds={rounds}, expected {min}..={max}"),
                full_detail: None,
                score: None,
            }
        }

        Criterion::CacheRateAbove {
            threshold,
            min_calls,
        } => {
            let effective_calls = if outcome.total_tool_calls > 0 {
                outcome.total_tool_calls
            } else {
                outcome.tool_calls_count
            };
            if effective_calls < *min_calls {
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: format!(
                        "too few tool calls: {effective_calls} < min_calls={min_calls} \
                         (cache rate requires at least {min_calls} calls)"
                    ),
                    full_detail: None,
                    score: None,
                };
            }
            if outcome.total_tool_calls == 0 && outcome.tool_calls_count == 0 {
                CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: true,
                    detail: "no tool calls — cache rate N/A (skip-pass)".into(),
                    full_detail: None,
                    score: None,
                }
            } else if outcome.total_tool_calls == 0 {
                // tool_calls_count > 0 but step_events weren't parsed.
                // This means step_events.jsonl was missing or unreadable.
                CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: format!(
                        "step_events missing: tool_calls_count={} but total_tool_calls=0 \
                         (cannot compute cache rate)",
                        outcome.tool_calls_count
                    ),
                    full_detail: None,
                    score: None,
                }
            } else {
                let rate = outcome.cache_hits as f64 / outcome.total_tool_calls as f64;
                let passed = rate >= *threshold;
                CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed,
                    detail: format!(
                        "cache_rate={:.1}% ({}/{}), threshold={:.0}%",
                        rate * 100.0,
                        outcome.cache_hits,
                        outcome.total_tool_calls,
                        threshold * 100.0
                    ),
                    full_detail: None,
                    score: None,
                }
            }
        }
    }
}

/// Scan stderr for `[fork-cache] {...}` JSON lines and return the
/// `outcome` field from each. Silently skips malformed lines and
/// lines where the outcome can't be found — a single corrupt event
/// should not hide the valid ones.
///
/// Field precedence: `outcome` (current wire name as serialized by
/// `astra_turn_core::fork_cache_event::ForkCacheEvent`) then `class`
/// (earlier harness-facing name; kept for YAML backward-compat). No
/// positional / first-key fallback — that was a schema-churn footgun
/// that would misclassify a re-tagged event.
fn parse_fork_cache_outcomes(stderr: &str) -> Vec<String> {
    let mut outcomes = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("[fork-cache]") else {
            continue;
        };
        let rest = rest.trim_start();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) else {
            continue;
        };
        // Only accept the two named fields. Unknown shapes are
        // skipped entirely — loud missing-outcome surface is
        // preferable to a silent misclassification.
        let name = v
            .get("outcome")
            .and_then(|c| c.as_str())
            .or_else(|| v.get("class").and_then(|c| c.as_str()));
        if let Some(s) = name {
            outcomes.push(s.to_string());
        }
    }
    outcomes
}

/// Reject a criterion whose bounds are internally inconsistent —
/// typos in YAML (`min: 5, max: 2`, `threshold: 2.0`, empty expect
/// list) otherwise turn into permanent-FAIL or permanent-PASS cases
/// that look like real bugs. Return `Err` with the precise field
/// name so the case author sees exactly what to change.
///
/// Called by `Case::from_path` at load time so the whole suite fails
/// fast on a typo rather than at runtime.
pub fn validate_criterion(c: &Criterion) -> Result<(), String> {
    match c {
        Criterion::ToolsCountBetween { min, max } => {
            if min > max {
                return Err(format!(
                    "ToolsCountBetween: min ({min}) > max ({max}); case will always FAIL"
                ));
            }
            Ok(())
        }
        Criterion::Judger { threshold, .. } => {
            if !threshold.is_finite() || *threshold < 0.0 || *threshold > 1.0 {
                return Err(format!(
                    "Judger.threshold must be finite in [0.0, 1.0]; got {threshold}"
                ));
            }
            Ok(())
        }
        Criterion::SessionEventCount {
            min, event_type, ..
        } => {
            if *min == 0 {
                return Err(format!(
                    "SessionEventCount.min must be >= 1 (min=0 is trivially-true for \
                     event_type={event_type:?}; did you mean >= 1?)"
                ));
            }
            Ok(())
        }
        Criterion::ForkCacheOutcome { expect } => {
            if expect.is_empty() {
                return Err(
                    "ForkCacheOutcome.expect must not be empty (no outcome would ever match)"
                        .into(),
                );
            }
            Ok(())
        }
        Criterion::StderrMatches { pattern } => {
            // Compile-check the regex at load so a bad pattern fails
            // parse, not every per-case evaluation.
            Regex::new(pattern)
                .map(|_| ())
                .map_err(|e| format!("StderrMatches.pattern: invalid regex {pattern:?}: {e}"))
        }
        Criterion::ToolCalled { name } | Criterion::JournalToolCalled { name, .. } => {
            if name.trim().is_empty() {
                return Err("tool name must not be empty".into());
            }
            Ok(())
        }
        Criterion::TextContains { needle } => {
            if needle.is_empty() {
                return Err("TextContains.needle must not be empty".into());
            }
            Ok(())
        }
        Criterion::ExitCode { .. } => Ok(()),
        Criterion::TokensBetween { min, max } => {
            if min > max {
                return Err(format!("TokensBetween: min ({min}) > max ({max})"));
            }
            Ok(())
        }
        Criterion::DurationBetween { min_ms, max_ms } => {
            if min_ms > max_ms {
                return Err(format!(
                    "DurationBetween: min_ms ({min_ms}) > max_ms ({max_ms})"
                ));
            }
            Ok(())
        }
        Criterion::ToolSequence { tools } => {
            if tools.is_empty() {
                return Err("ToolSequence.tools must not be empty".into());
            }
            for (i, t) in tools.iter().enumerate() {
                if t.trim().is_empty() {
                    return Err(format!("ToolSequence.tools[{i}] must not be empty"));
                }
            }
            Ok(())
        }
        Criterion::TurnRoundsBetween { min, max } => {
            if min > max {
                return Err(format!("TurnRoundsBetween: min ({min}) > max ({max})"));
            }
            Ok(())
        }
        Criterion::CacheRateAbove { threshold, .. } => {
            if !threshold.is_finite() || *threshold < 0.0 || *threshold > 1.0 {
                return Err(format!(
                    "CacheRateAbove.threshold must be in [0.0, 1.0]; got {threshold}"
                ));
            }
            Ok(())
        }
    }
}

/// Validate every criterion in a list. Returns the first offender's
/// error with a 1-based index so the case author can find the line.
pub fn validate_criteria(criteria: &[Criterion]) -> Result<(), String> {
    for (i, c) in criteria.iter().enumerate() {
        validate_criterion(c).map_err(|e| format!("criteria[{}]: {e}", i + 1))?;
    }
    Ok(())
}

/// True when every non-Judger criterion passed. Used by the runner
/// to decide whether to even bother invoking the LLM judger — if a
/// deterministic check already failed, the case is known-FAIL and
/// the judger call would waste a provider round-trip.
///
/// The slice length of `results` must equal `criteria.len()` (they
/// come out of `evaluate_deterministic`). Mismatched lengths return
/// `false` conservatively.
pub fn non_judger_all_pass(criteria: &[Criterion], results: &[CriterionResult]) -> bool {
    if results.len() != criteria.len() {
        return false;
    }
    criteria
        .iter()
        .zip(results.iter())
        .filter(|(c, _)| !matches!(c, Criterion::Judger { .. }))
        .all(|(_, r)| r.passed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunOutcome;

    fn outcome_with_tools(tools: &[&str]) -> RunOutcome {
        RunOutcome {
            model: "m".into(),
            exit_code: 0,
            text: "ok".into(),
            stderr: String::new(),
            session_id: None,
            run_id: None,
            tool_calls_count: tools.len() as u32,
            tools_used: tools.iter().map(|s| s.to_string()).collect(),
            completion_tokens: 0,
            prompt_tokens: 0,
            duration_ms: 0,
            turn_rounds: 0,
            cache_hits: 0,
            total_tool_calls: 0,
            ttft_ms: 0,
        }
    }

    #[test]
    fn tool_called_pass_fail() {
        let out = outcome_with_tools(&["spawn_agent", "read_file"]);
        let hit = evaluate_deterministic(
            &[Criterion::ToolCalled {
                name: "spawn_agent".into(),
            }],
            &out,
        );
        assert!(hit[0].passed);
        let miss = evaluate_deterministic(
            &[Criterion::ToolCalled {
                name: "nonexistent".into(),
            }],
            &out,
        );
        assert!(!miss[0].passed);
    }

    #[test]
    fn stderr_matches_uses_regex() {
        let mut out = outcome_with_tools(&[]);
        out.stderr = "some noise\n[fork-cache] {...}\nmore noise".into();
        let r = evaluate_deterministic(
            &[Criterion::StderrMatches {
                pattern: r"^\[fork-cache\]".into(),
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn stderr_matches_invalid_regex_fails_safely() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::StderrMatches {
                pattern: "(".into(),
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("invalid regex"));
    }

    #[test]
    fn tools_count_range_inclusive() {
        let out = outcome_with_tools(&["a", "b", "c"]);
        let inside =
            evaluate_deterministic(&[Criterion::ToolsCountBetween { min: 1, max: 3 }], &out);
        assert!(inside[0].passed);
        let outside =
            evaluate_deterministic(&[Criterion::ToolsCountBetween { min: 5, max: 10 }], &out);
        assert!(!outside[0].passed);
    }

    #[test]
    fn judger_variant_not_evaluated_synchronously() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::Judger {
                question: "ok?".into(),
                threshold: 0.7,
                model: None,
            }],
            &out,
        );
        // Placeholder-failed; caller runs the async judger separately.
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("judger"));
    }

    // Regression: the judger placeholder `passed=false` must NOT
    // gate the judger from running. `non_judger_all_pass` only
    // considers the deterministic checks. Before this fix,
    // every Judger-bearing case short-circuited to FAIL and the
    // judger never ran.
    #[test]
    fn non_judger_all_pass_ignores_judger_placeholder() {
        let out = outcome_with_tools(&["Read"]);
        let criteria = vec![
            Criterion::ExitCode { code: 0 },
            Criterion::ToolCalled {
                name: "Read".into(),
            },
            Criterion::Judger {
                question: "ok?".into(),
                threshold: 0.7,
                model: None,
            },
        ];
        let results = evaluate_deterministic(&criteria, &out);
        assert!(non_judger_all_pass(&criteria, &results));
    }

    #[test]
    fn non_judger_all_pass_fails_when_deterministic_fails() {
        let out = outcome_with_tools(&[]);
        let criteria = vec![
            Criterion::ToolCalled {
                name: "nonexistent".into(),
            },
            Criterion::Judger {
                question: "ok?".into(),
                threshold: 0.7,
                model: None,
            },
        ];
        let results = evaluate_deterministic(&criteria, &out);
        assert!(!non_judger_all_pass(&criteria, &results));
    }

    fn mk_session(events: &[(&str, serde_json::Value)]) -> SessionCapture {
        use crate::session_capture::JournalEvent;
        SessionCapture {
            session_id: "s".into(),
            journal_path: std::path::PathBuf::from("/x"),
            skipped_lines: 0,
            events: events
                .iter()
                .map(|(t, raw)| JournalEvent {
                    event_type: (*t).to_string(),
                    raw: raw.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn session_event_count_passes_when_min_met() {
        let sess = mk_session(&[
            ("llm_round", serde_json::json!({})),
            ("llm_round", serde_json::json!({})),
            ("tool_invocation", serde_json::json!({})),
        ]);
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic_with_session(
            &[Criterion::SessionEventCount {
                event_type: "llm_round".into(),
                min: 2,
                optional: false,
            }],
            &out,
            Some(&sess),
        );
        assert!(r[0].passed);
    }

    #[test]
    fn session_event_count_fails_by_default_when_no_capture() {
        // Pre-fix semantics: this used to skip-pass, which masked
        // genuine session-capture failures. New default: FAIL with
        // an actionable hint. `optional: true` opts back into skip.
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::SessionEventCount {
                event_type: "llm_round".into(),
                min: 2,
                optional: false,
            }],
            &out,
        );
        assert!(!r[0].passed, "default must FAIL, not skip");
        assert!(r[0].detail.contains("FAILED"));
        assert!(r[0].detail.contains("optional: true"));
    }

    #[test]
    fn session_event_count_optional_skips_when_no_capture() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::SessionEventCount {
                event_type: "llm_round".into(),
                min: 2,
                optional: true,
            }],
            &out,
        );
        assert!(r[0].passed, "optional=true must skip-pass");
        assert!(r[0].detail.contains("skipped"));
    }

    #[test]
    fn journal_tool_called_reads_from_session_not_envelope() {
        // RunOutcome.tools_used is empty but journal shows Read was
        // invoked — journal is the source of truth (subagent calls
        // may not flow back through the envelope).
        let sess = mk_session(&[(
            "tool_invocation",
            serde_json::json!({"metadata": {"tool_name": "Read"}}),
        )]);
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic_with_session(
            &[Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: false,
            }],
            &out,
            Some(&sess),
        );
        assert!(r[0].passed);
    }

    #[test]
    fn journal_tool_called_fails_when_tool_missing() {
        let sess = mk_session(&[(
            "tool_invocation",
            serde_json::json!({"metadata": {"tool_name": "Read"}}),
        )]);
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic_with_session(
            &[Criterion::JournalToolCalled {
                name: "Grep".into(),
                optional: false,
            }],
            &out,
            Some(&sess),
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("NOT invoked"));
    }

    #[test]
    fn journal_tool_called_fails_by_default_when_no_capture() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: false,
            }],
            &out,
        );
        assert!(!r[0].passed, "default must FAIL");
        assert!(r[0].detail.contains("FAILED"));
    }

    #[test]
    fn journal_tool_called_optional_skips_when_no_capture() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: true,
            }],
            &out,
        );
        assert!(r[0].passed);
        assert!(r[0].detail.contains("skipped"));
    }

    #[test]
    fn non_judger_all_pass_false_on_length_mismatch() {
        // Defensive guard — callers should never pass mismatched
        // slices, but if they do we must NOT proceed to run a judger
        // against a case whose results we can't align.
        let criteria = vec![Criterion::ExitCode { code: 0 }];
        let results: Vec<CriterionResult> = vec![];
        assert!(!non_judger_all_pass(&criteria, &results));
    }

    // ── validate_criterion / validate_criteria (R3 #2) ──

    #[test]
    fn validate_tools_count_between_rejects_inverted_range() {
        let err = validate_criterion(&Criterion::ToolsCountBetween { min: 5, max: 2 })
            .expect_err("min>max should fail");
        assert!(err.contains("min (5) > max (2)"), "err = {err}");
    }

    #[test]
    fn validate_tools_count_between_accepts_equal_bounds() {
        // min == max means "exactly N calls" — a legitimate assertion.
        assert!(validate_criterion(&Criterion::ToolsCountBetween { min: 3, max: 3 }).is_ok());
    }

    #[test]
    fn validate_judger_rejects_out_of_range_threshold() {
        for t in [-0.1, 1.5, f64::NAN, f64::INFINITY] {
            let c = Criterion::Judger {
                question: "q".into(),
                threshold: t,
                model: None,
            };
            assert!(
                validate_criterion(&c).is_err(),
                "threshold {t} should fail validation"
            );
        }
    }

    #[test]
    fn validate_judger_accepts_boundary_thresholds() {
        for t in [0.0, 0.5, 1.0] {
            let c = Criterion::Judger {
                question: "q".into(),
                threshold: t,
                model: None,
            };
            assert!(validate_criterion(&c).is_ok(), "threshold {t} should pass");
        }
    }

    #[test]
    fn validate_session_event_count_rejects_min_zero() {
        let err = validate_criterion(&Criterion::SessionEventCount {
            event_type: "llm_round".into(),
            min: 0,
            optional: false,
        })
        .expect_err("min=0 is trivially-true — should reject");
        assert!(err.contains("min must be >= 1"));
    }

    #[test]
    fn validate_fork_cache_outcome_rejects_empty_expect() {
        let err = validate_criterion(&Criterion::ForkCacheOutcome { expect: vec![] })
            .expect_err("empty expect should fail");
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn validate_stderr_matches_rejects_bad_regex() {
        let err = validate_criterion(&Criterion::StderrMatches {
            pattern: "(".into(),
        })
        .expect_err("bad regex should fail at load");
        assert!(err.contains("invalid regex"));
    }

    #[test]
    fn validate_rejects_empty_tool_name_and_empty_needle() {
        assert!(validate_criterion(&Criterion::ToolCalled { name: "   ".into() }).is_err());
        assert!(
            validate_criterion(&Criterion::JournalToolCalled {
                name: "".into(),
                optional: false
            })
            .is_err()
        );
        assert!(validate_criterion(&Criterion::TextContains { needle: "".into() }).is_err());
    }

    #[test]
    fn validate_criteria_reports_one_based_index_for_first_offender() {
        let criteria = vec![
            Criterion::ExitCode { code: 0 },
            Criterion::ToolsCountBetween { min: 5, max: 2 },
            Criterion::ExitCode { code: 1 }, // would also pass validation
        ];
        let err = validate_criteria(&criteria).expect_err("second criterion is bad");
        assert!(err.contains("criteria[2]"), "1-based index expected: {err}");
        assert!(err.contains("min (5) > max (2)"));
    }

    // ── ForkCacheOutcome tests ──

    fn outcome_with_stderr(stderr: &str) -> RunOutcome {
        let mut out = outcome_with_tools(&[]);
        out.stderr = stderr.to_string();
        out
    }

    #[test]
    fn fork_cache_outcome_passes_on_real_wire_shape() {
        // Real wire shape emitted by
        // `astra_turn_core::fork_cache_event::StderrForkCacheSink`.
        // The field is `outcome`, rename_all = snake_case.
        let out = outcome_with_stderr(
            "[fork-cache] {\"prefix_id\":\"pfx-1\",\"outcome\":\"hit\",\"ratio\":0.9}",
        );
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn fork_cache_outcome_accepts_legacy_class_alias() {
        // Back-compat: older harness tooling + a brief YAML window
        // used `class` as the field name. The parser still accepts
        // it so a rename on the consumer side doesn't silently
        // break cases that predate the rename.
        let out = outcome_with_stderr("[fork-cache] {\"class\":\"hit\"}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn fork_cache_outcome_rejects_unknown_shape_instead_of_guessing() {
        // Regression: the previous implementation fell back to "first
        // object key" for events it couldn't parse, which silently
        // misclassified `{"metadata":{...}}` or similar as a valid
        // class. Now: unknown shapes produce NO outcome, and the
        // criterion reports zero events seen rather than guessing.
        let out = outcome_with_stderr("[fork-cache] {\"prefix_id\":\"x\",\"metadata\":{}}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into(), "partial_drift".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(
            r[0].detail.contains("no [fork-cache]"),
            "unknown-shape events must not be fabricated into outcomes; detail = {:?}",
            r[0].detail
        );
    }

    #[test]
    fn fork_cache_outcome_fails_when_only_other_outcomes_seen() {
        let out = outcome_with_stderr("[fork-cache] {\"outcome\":\"miss\"}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("miss"));
    }

    #[test]
    fn fork_cache_outcome_fails_when_no_events_seen() {
        let out = outcome_with_stderr("unrelated noise");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("no [fork-cache]"));
    }

    #[test]
    fn fork_cache_outcome_ignores_malformed_event_and_uses_good_ones() {
        let out = outcome_with_stderr(
            "[fork-cache] this is not json\n[fork-cache] {\"outcome\":\"hit\"}\n",
        );
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(
            r[0].passed,
            "one malformed event must not mask a later valid hit"
        );
    }

    #[test]
    fn fork_cache_outcome_accepts_any_of_multiple_expected_values() {
        let out = outcome_with_stderr("[fork-cache] {\"outcome\":\"partial_drift\"}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into(), "partial_drift".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn tokens_between_passes_in_range() {
        let mut out = RunOutcome::new("m");
        out.prompt_tokens = 100;
        out.completion_tokens = 200;
        let r = evaluate_deterministic(&[Criterion::TokensBetween { min: 200, max: 400 }], &out);
        assert!(r[0].passed, "{}", r[0].detail);
    }

    #[test]
    fn tokens_between_fails_over_max() {
        let mut out = RunOutcome::new("m");
        out.prompt_tokens = 5000;
        out.completion_tokens = 5000;
        let r = evaluate_deterministic(&[Criterion::TokensBetween { min: 0, max: 1000 }], &out);
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("10000"));
    }

    #[test]
    fn duration_between_passes_in_range() {
        let mut out = RunOutcome::new("m");
        out.duration_ms = 5000;
        let r = evaluate_deterministic(
            &[Criterion::DurationBetween {
                min_ms: 1000,
                max_ms: 10000,
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn duration_between_fails_too_slow() {
        let mut out = RunOutcome::new("m");
        out.duration_ms = 60000;
        let r = evaluate_deterministic(
            &[Criterion::DurationBetween {
                min_ms: 0,
                max_ms: 30000,
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("60000"));
    }

    #[test]
    fn tool_sequence_passes_subsequence() {
        let out = RunOutcome::new("m").with_tools_used(vec![
            "bash".into(),
            "read_file".into(),
            "bash".into(),
            "str_replace".into(),
        ]);
        let r = evaluate_deterministic(
            &[Criterion::ToolSequence {
                tools: vec!["read_file".into(), "str_replace".into()],
            }],
            &out,
        );
        assert!(r[0].passed, "{}", r[0].detail);
    }

    #[test]
    fn tool_sequence_fails_wrong_order() {
        let out =
            RunOutcome::new("m").with_tools_used(vec!["str_replace".into(), "read_file".into()]);
        let r = evaluate_deterministic(
            &[Criterion::ToolSequence {
                tools: vec!["read_file".into(), "str_replace".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
    }

    #[test]
    fn tool_sequence_fails_missing_tool() {
        let out = RunOutcome::new("m").with_tools_used(vec!["bash".into()]);
        let r = evaluate_deterministic(
            &[Criterion::ToolSequence {
                tools: vec!["read_file".into(), "str_replace".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("matched 0/2"));
    }

    #[test]
    fn turn_rounds_between_passes() {
        let mut out = RunOutcome::new("m");
        out.turn_rounds = 2;
        let r = evaluate_deterministic(&[Criterion::TurnRoundsBetween { min: 1, max: 3 }], &out);
        assert!(r[0].passed, "{}", r[0].detail);
    }

    #[test]
    fn turn_rounds_between_fails_too_many() {
        let mut out = RunOutcome::new("m");
        out.turn_rounds = 10;
        let r = evaluate_deterministic(&[Criterion::TurnRoundsBetween { min: 1, max: 3 }], &out);
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("10"));
    }

    #[test]
    fn cache_rate_above_passes() {
        let mut out = RunOutcome::new("m");
        out.total_tool_calls = 10;
        out.cache_hits = 8;
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.5,
                min_calls: 1,
            }],
            &out,
        );
        assert!(r[0].passed, "{}", r[0].detail);
    }

    #[test]
    fn cache_rate_above_fails() {
        let mut out = RunOutcome::new("m");
        out.total_tool_calls = 10;
        out.cache_hits = 1;
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.5,
                min_calls: 1,
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("10.0%"));
    }

    #[test]
    fn cache_rate_above_fails_when_no_tools_and_min_calls_set() {
        let out = RunOutcome::new("m");
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.9,
                min_calls: 1,
            }],
            &out,
        );
        assert!(!r[0].passed, "min_calls=1 with 0 tool calls must FAIL");
        assert!(r[0].detail.contains("too few tool calls"));
    }

    #[test]
    fn cache_rate_above_skip_passes_no_tools_when_min_calls_zero() {
        let out = RunOutcome::new("m");
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.9,
                min_calls: 0,
            }],
            &out,
        );
        assert!(r[0].passed, "min_calls=0 with no tools should skip-pass");
    }

    #[test]
    fn cache_rate_above_fails_when_step_events_missing() {
        let mut out = RunOutcome::new("m");
        out.tool_calls_count = 3; // envelope says tools were called
        out.total_tool_calls = 0; // but step_events weren't parsed
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.5,
                min_calls: 1,
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("step_events missing"));
    }
}

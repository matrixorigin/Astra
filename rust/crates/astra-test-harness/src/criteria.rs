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
    /// event appears"). SKIPS (passes) when no session is loaded
    /// so offline smoke runs don't blanket-fail — explicit opt-in
    /// via `debug_log: true` or `--capture-session`.
    SessionEventCount {
        event_type: String,
        #[serde(default = "default_event_min")]
        min: u32,
    },

    /// Passes when the given tool name appears in the journal's
    /// `tool_invocation` events. Journal is the source of truth
    /// for tool calls — `tools_used` from the CLI envelope may
    /// miss tools emitted inside sub-agent runs. SKIPS when no
    /// session is loaded (see `SessionEventCount`).
    JournalToolCalled { name: String },

    /// Passes when at least one `[fork-cache]` JSON event in stderr
    /// has its `class` field in `expect`. Deterministic alternative
    /// to letting a judger classify the event from prose — pins the
    /// exact runtime contract (class is one of hit / miss /
    /// partial_drift / validation_failed / fallback).
    ///
    /// Example:
    /// ```yaml
    /// - type: fork_cache_class
    ///   expect: [hit]
    /// ```
    ForkCacheClass {
        /// Accepted class names. A stderr event whose `class` equals
        /// any of these passes. Case-sensitive match against the
        /// serialized enum name in the event.
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
}

fn default_judger_threshold() -> f64 {
    0.7
}

fn default_event_min() -> u32 {
    1
}

/// Result of evaluating a single criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion: Criterion,
    pub passed: bool,
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
        Criterion::SessionEventCount { event_type, min } => {
            let Some(sess) = session else {
                return CriterionResult {
                    criterion: c.clone(),
                    passed: true,
                    detail: format!(
                        "session_event_count {event_type} skipped (no session capture; enable with debug_log: true or --capture-session)"
                    ),
                    full_detail: None,
                score: None,
                };
            };
            let n = sess.count_events(event_type);
            let pass = n as u32 >= *min;
            CriterionResult {
                criterion: c.clone(),
                passed: pass,
                detail: format!(
                    "session events type={event_type} count={n} (expected >= {min})"
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolCalled { name } => {
            let Some(sess) = session else {
                return CriterionResult {
                    criterion: c.clone(),
                    passed: true,
                    detail: format!(
                        "journal_tool_called {name} skipped (no session capture)"
                    ),
                    full_detail: None,
                score: None,
                };
            };
            let tools = sess.tools_invoked();
            let hit = tools.iter().any(|t| t == name);
            CriterionResult {
                criterion: c.clone(),
                passed: hit,
                detail: if hit {
                    format!("journal tool {name} was invoked")
                } else {
                    format!(
                        "journal tool {name} NOT invoked (journal tools: {tools:?})"
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::ForkCacheClass { expect } => {
            let hits = parse_fork_cache_classes(&outcome.stderr);
            let pass = hits.iter().any(|c| expect.iter().any(|e| e == c));
            CriterionResult {
                criterion: c.clone(),
                passed: pass,
                detail: if pass {
                    format!(
                        "fork-cache event with class in {expect:?} observed (all seen: {hits:?})"
                    )
                } else if hits.is_empty() {
                    "no [fork-cache] events observed in stderr".to_string()
                } else {
                    format!(
                        "no [fork-cache] event matched {expect:?}; seen classes: {hits:?}"
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::Judger { .. } => CriterionResult {
            criterion: c.clone(),
            passed: false,
            detail: "judger not yet evaluated (handled by runner)".into(),
            full_detail: None,
            score: None,
        },
    }
}

/// Scan stderr for `[fork-cache] {...}` JSON lines and return the
/// `class` field from each. Silently skips malformed lines — a
/// single corrupt event should not hide the valid ones.
fn parse_fork_cache_classes(stderr: &str) -> Vec<String> {
    let mut classes = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("[fork-cache]") else {
            continue;
        };
        let rest = rest.trim_start();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) else {
            continue;
        };
        // The event may carry `class` directly or wrap it in a
        // discriminator object. Prefer direct `class` field.
        if let Some(s) = v.get("class").and_then(|c| c.as_str()) {
            classes.push(s.to_string());
        } else if let Some(obj) = v.as_object() {
            // Fallback: the runtime's ForkCacheEvent serializes as a
            // tagged enum, so the top-level key names the variant
            // (e.g. `{"hit": {...}}`, `{"partial_drift": {...}}`).
            // Accept the first key as the class when nothing else
            // presents itself.
            if let Some(k) = obj.keys().next() {
                classes.push(k.clone());
            }
        }
    }
    classes
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
        let inside = evaluate_deterministic(
            &[Criterion::ToolsCountBetween { min: 1, max: 3 }],
            &out,
        );
        assert!(inside[0].passed);
        let outside = evaluate_deterministic(
            &[Criterion::ToolsCountBetween { min: 5, max: 10 }],
            &out,
        );
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
            }],
            &out,
            Some(&sess),
        );
        assert!(r[0].passed);
    }

    #[test]
    fn session_event_count_skips_when_no_capture() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::SessionEventCount {
                event_type: "llm_round".into(),
                min: 2,
            }],
            &out,
        );
        assert!(r[0].passed);
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
            }],
            &out,
            Some(&sess),
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("NOT invoked"));
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

    // ── ForkCacheClass tests ──

    fn outcome_with_stderr(stderr: &str) -> RunOutcome {
        let mut out = outcome_with_tools(&[]);
        out.stderr = stderr.to_string();
        out
    }

    #[test]
    fn fork_cache_class_passes_on_direct_class_field() {
        let out =
            outcome_with_stderr("noise\n[fork-cache] {\"class\":\"hit\",\"ratio\":0.9}\nmore");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheClass {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn fork_cache_class_passes_on_tagged_enum_shape() {
        // When the runtime ships the event as a tagged enum
        // `{"partial_drift": {...}}`, the first key names the class.
        let out = outcome_with_stderr(
            "[fork-cache] {\"partial_drift\":{\"changed_tools\":[\"spawn_agent\"]}}",
        );
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheClass {
                expect: vec!["partial_drift".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn fork_cache_class_fails_when_only_other_classes_seen() {
        let out = outcome_with_stderr("[fork-cache] {\"class\":\"miss\"}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheClass {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("miss"));
    }

    #[test]
    fn fork_cache_class_fails_when_no_events_seen() {
        let out = outcome_with_stderr("unrelated noise");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheClass {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("no [fork-cache]"));
    }

    #[test]
    fn fork_cache_class_ignores_malformed_event_and_uses_good_ones() {
        let out = outcome_with_stderr(
            "[fork-cache] this is not json\n[fork-cache] {\"class\":\"hit\"}\n",
        );
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheClass {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(r[0].passed, "one malformed event must not mask a later valid hit");
    }

    #[test]
    fn fork_cache_class_accepts_any_of_multiple_expected_classes() {
        // Soft-fallback contract: "validation_failed" OR "fallback"
        // both satisfy a provider-mismatch case. Expect list is OR.
        let out = outcome_with_stderr("[fork-cache] {\"class\":\"validation_failed\"}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheClass {
                expect: vec!["fallback".into(), "validation_failed".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }
}

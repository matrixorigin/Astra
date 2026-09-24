use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::tool::args::shape::{tool_call_arguments_value, tool_call_name};
use crate::tool::result::semantics::canonical_tool_identity_parts;

/// A stall-equivalence key, not an execution identity or permission to reuse a
/// result. Producers retain their own argument normalization rules; retained
/// detector state never needs the original arguments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StallSignature {
    tool_name: String,
    digest: [u8; 32],
}

impl StallSignature {
    pub fn new(tool_name: &str, canonical_bytes: &[u8]) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"astra.stall.signature.v1\0");
        hash.update((tool_name.len() as u64).to_be_bytes());
        hash.update(tool_name.as_bytes());
        hash.update(canonical_bytes);
        Self {
            tool_name: tool_name.to_owned(),
            digest: hash.finalize().into(),
        }
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Diagnostic projection only; never parse this back into detector state.
    pub fn display_hint(&self) -> String {
        let digest: String = self.digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{}:{digest}", self.tool_name)
    }
}

/// Errors from stall / divergence detection (invalid configuration or inputs).
#[derive(Debug, Clone, Error, PartialEq)]
pub enum StallDetectionError {
    #[error("stall window or exploration budget must be > 0 (got {0})")]
    InvalidWindowOrBudget(usize),
}

/// Require 3 consecutive identical tool call turns (not 2) to detect stall.
/// Window=2 was too aggressive: legitimate retries and exploration patterns
/// (e.g. read_file with different args each turn) triggered false stalls.
pub const SERVER_STALL_WINDOW: usize = 3;

/// When this many consecutive rounds emit the exact same tool-call
/// signature, the pattern is worth surfacing as advisory evidence. Repetition
/// alone does not prove that the next call is invalid or that the turn should
/// stop; actual resource ceilings remain separate.
///
/// This threshold leaves room for legitimate retries while retaining an
/// observable repetition signal; it is independent of correction counters.
pub const CONSECUTIVE_IDENTICAL_SIGS_ADVISORY_THRESHOLD: usize = 5;

/// Count how many of the most recent `turn_sigs` entries share the same
/// full `name+args` signature set. Used to decide whether we've crossed
/// the [`CONSECUTIVE_IDENTICAL_SIGS_ADVISORY_THRESHOLD`] threshold.
#[must_use]
pub fn trailing_identical_sig_depth(turn_sigs: &[BTreeSet<crate::stall::StallSignature>]) -> usize {
    let Some(last) = turn_sigs.last() else {
        return 0;
    };
    // Trivial/degenerate inputs (empty sig set) don't represent real
    // tool activity and shouldn't count as a stall signal.
    if last.is_empty() {
        return 0;
    }
    let mut count = 1usize;
    for prev in turn_sigs.iter().rev().skip(1) {
        if prev == last {
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// User-visible error prefix when the agentic loop exhausts the per-request remaining-turn budget.
/// Call sites append the actual budget number, e.g. `format!("{} (budget: {} turns)", MSG, n)`.
pub const CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG: &str = "Turn budget exhausted. To increase, set ASTRA_MAX_TURNS (interactive) or ASTRA_PLAN_SUBTASK_MAX_TURNS (plan subtasks).";

/// Default window for signature-diversity observations, not a limit on
/// exploration or an authority to inject corrections.
pub const MAX_EXPLORATION_ROUNDS: usize = 3;

pub fn canonical_tool_args(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| raw.to_string()),
        Err(_) => raw.to_string(),
    }
}

pub fn server_tool_call_signature(tool_calls: &[Value]) -> BTreeSet<crate::stall::StallSignature> {
    tool_calls
        .iter()
        .map(|tool_call| {
            // Support both formats:
            //   Nested (OpenAI): {function: {name, arguments}}
            //   Flat (internal): {name, arguments}
            let (name, arguments) =
                if let Some(function) = tool_call.get("function").and_then(Value::as_object) {
                    let n = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let a = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    (n.to_string(), a.to_string())
                } else {
                    let n = tool_call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let a = tool_call
                        .get("arguments")
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                        .unwrap_or_default();
                    (n.to_string(), a)
                };
            StallSignature::new(&name, canonical_tool_args(&arguments).as_bytes())
        })
        .collect()
}

pub fn record_server_tool_signatures(
    tool_sigs: &mut Vec<BTreeSet<crate::stall::StallSignature>>,
    tool_calls: &[Value],
    window: usize,
) {
    if tool_calls.is_empty() {
        return;
    }

    tool_sigs.push(server_tool_call_signature(tool_calls));
    if tool_sigs.len() > window {
        let drain_count = tool_sigs.len() - window;
        tool_sigs.drain(0..drain_count);
    }
}

/// Detect exact-repetition stall: same tool calls with same args repeated N times.
pub fn detect_server_stall(
    tool_sigs: &[BTreeSet<crate::stall::StallSignature>],
    window: usize,
) -> Result<bool, StallDetectionError> {
    if window == 0 {
        return Err(StallDetectionError::InvalidWindowOrBudget(0));
    }
    if tool_sigs.len() < window {
        return Ok(false);
    }

    let recent = &tool_sigs[tool_sigs.len() - window..];
    Ok(recent.iter().all(|sig| sig == &recent[window - 1]))
}

// ─── CLI stream_chat_sse agentic loop (astra) ──────────────────────────────

/// Subtract this many "remaining inner-loop turns" when TurnGuard reports **critical** during the
/// CLI `/chat/turn` agentic loop (`apply_post_tool_turn_policy`).
pub const CLI_AGENTIC_VERDICT_REMAINING_PENALTY_CRITICAL: usize = 5;
/// Same for **warning** severity.
pub const CLI_AGENTIC_VERDICT_REMAINING_PENALTY_WARNING: usize = 2;

/// Per-round signature set and tool-name set for astra flat `tool_calls` rows (`name` + `arguments` JSON).
pub fn round_tool_call_sig_and_names(
    tool_calls: &[Value],
) -> (BTreeSet<crate::stall::StallSignature>, HashSet<String>) {
    let sig_set: BTreeSet<crate::stall::StallSignature> = tool_calls
        .iter()
        .map(|tc| {
            let name = tool_call_name(tc).unwrap_or("");
            let args = tool_call_arguments_value(tc);
            let (name, normalized) = canonical_tool_identity_parts(name, &args);
            let canonical =
                serde_json::to_vec(&normalized).expect("JSON values serialize without failure");
            StallSignature::new(&name, &canonical)
        })
        .collect();
    let name_set: HashSet<String> = tool_calls
        .iter()
        .map(|tc| tool_call_name(tc).unwrap_or("").to_string())
        .collect();
    (sig_set, name_set)
}

/// True when the last `window` rounds have **identical** tool-call signatures
/// (name + args). This is the CLI-loop equivalent of [`detect_server_stall`].
///
/// Unlike tool-name sets, signatures distinguish calls with different
/// arguments. Neither changing nor repeated arguments establish task progress.
pub fn detect_cli_tool_sig_stall(
    turn_sigs: &[BTreeSet<crate::stall::StallSignature>],
    window: usize,
) -> Result<bool, StallDetectionError> {
    detect_server_stall(turn_sigs, window)
}

// ─── Divergence detection ───────────────────────────────────────────────────

/// Result of divergence analysis for the current turn sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum DivergenceStatus {
    /// Sufficient signature diversity, or insufficient evidence to classify.
    Healthy,
    /// Low signature diversity in the observed window.
    Exploring(usize),
    /// Identical signature sets across the observed window.
    Diverging(usize),
}

/// Signature-diversity assessment across recent rounds, not an assessment of
/// result novelty or task success. These observations carry no correction authority.
///
/// - `NoProgress` — identical call-signature sets throughout the window.
/// - `LowNovelty(rate)` — distinct-signature rate below `NOVELTY_FLOOR`.
/// - `Healthy` — enough signature diversity, or insufficient evidence.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressStatus {
    Healthy,
    LowNovelty(f32),
    NoProgress,
}

/// Floor below which signature novelty is considered "low". Chosen so
/// that two-tool rotation with distinct args per call scores above it,
/// while 3 rounds of a single repeated tool scores below.
pub const NOVELTY_FLOOR: f32 = 0.34;

/// Total number of individual tool-call signatures across the last
/// `window` rounds. Used as the denominator for novelty rate.
fn total_sig_count(rounds: &[BTreeSet<crate::stall::StallSignature>]) -> usize {
    rounds.iter().map(|r| r.len()).sum()
}

/// Union of all signatures across the last `window` rounds.
fn distinct_sig_count(rounds: &[BTreeSet<crate::stall::StallSignature>]) -> usize {
    let mut seen = BTreeSet::new();
    for r in rounds {
        for s in r {
            seen.insert(s.clone());
        }
    }
    seen.len()
}

/// Task-agnostic signature assessment. This does not judge tool choice,
/// result freshness, or whether repetition is required for recovery.
pub fn assess_progress(
    tool_sigs: &[BTreeSet<crate::stall::StallSignature>],
    window: usize,
) -> Result<ProgressStatus, StallDetectionError> {
    if window == 0 {
        return Err(StallDetectionError::InvalidWindowOrBudget(0));
    }
    if tool_sigs.len() < window {
        return Ok(ProgressStatus::Healthy);
    }
    let recent = &tool_sigs[tool_sigs.len() - window..];

    // Rounds with no tool calls don't count as progress signal either way.
    if recent.iter().any(|r| r.is_empty()) {
        return Ok(ProgressStatus::Healthy);
    }

    // Exact-repetition: every round has the same signature set.
    if recent.iter().all(|r| r == &recent[0]) {
        return Ok(ProgressStatus::NoProgress);
    }

    let total = total_sig_count(recent);
    let distinct = distinct_sig_count(recent);
    if total == 0 {
        return Ok(ProgressStatus::Healthy);
    }
    let novelty = distinct as f32 / total as f32;
    if novelty < NOVELTY_FLOOR {
        Ok(ProgressStatus::LowNovelty(novelty))
    } else {
        Ok(ProgressStatus::Healthy)
    }
}

/// Detect divergence using the default exploration window.
///
/// Delegates to [`assess_progress`]: `NoProgress` maps to `Diverging`,
/// `LowNovelty` to `Exploring`, and `Healthy` to `Healthy`.
/// All three are signature observations, not instructions to the model.
pub fn detect_divergence(
    tool_sigs: &[BTreeSet<crate::stall::StallSignature>],
) -> Result<DivergenceStatus, StallDetectionError> {
    detect_divergence_with_window(tool_sigs, MAX_EXPLORATION_ROUNDS)
}

pub fn detect_divergence_with_window(
    tool_sigs: &[BTreeSet<crate::stall::StallSignature>],
    exploration_round_window: usize,
) -> Result<DivergenceStatus, StallDetectionError> {
    match assess_progress(tool_sigs, exploration_round_window)? {
        ProgressStatus::Healthy => Ok(DivergenceStatus::Healthy),
        ProgressStatus::LowNovelty(_) => {
            // Report as Exploring (hint-only); callers should NOT inject
            // a correction — the agent may be doing legitimate analysis.
            Ok(DivergenceStatus::Exploring(exploration_round_window))
        }
        ProgressStatus::NoProgress => Ok(DivergenceStatus::Diverging(exploration_round_window)),
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    #[test]
    fn stall_signature_retains_equivalence_without_retaining_arguments() {
        let signature = StallSignature::new("bash", b"private-argument-sentinel");
        assert_eq!(signature.tool_name(), "bash");
        assert_eq!(
            signature,
            StallSignature::new("bash", b"private-argument-sentinel")
        );
        assert_ne!(signature, StallSignature::new("bash", b"different"));
        assert_ne!(
            signature,
            StallSignature::new("read_file", b"private-argument-sentinel")
        );
        // Length framing prevents ambiguity between the name and argument bytes.
        assert_ne!(
            StallSignature::new("ab", b"c"),
            StallSignature::new("a", b"bc")
        );
        let wire = serde_json::to_value(&signature).unwrap();
        assert!(!wire.to_string().contains("private-argument-sentinel"));
        assert_eq!(
            serde_json::from_value::<StallSignature>(wire.clone()).unwrap(),
            signature
        );
        for field in ["tool_name", "digest"] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<StallSignature>(missing).is_err());
        }
        let mut unknown = wire;
        unknown["arguments"] = serde_json::json!("unexpected");
        assert!(serde_json::from_value::<StallSignature>(unknown).is_err());
    }

    #[test]
    fn typed_stall_producers_preserve_old_equivalence_classes() {
        let calls = [
            serde_json::json!({"name":"read_file","arguments":{"path":"private-argument-sentinel","limit":1}}),
            serde_json::json!({"function":{"name":"read_file","arguments":"{\"limit\":1,\"path\":\"private-argument-sentinel\"}"}}),
            serde_json::json!({"name":"read_file","arguments":{"path":"different","limit":1}}),
            serde_json::json!({"name":"bash","arguments":{"command":"git diff -- src/"}}),
            serde_json::json!({"name":"git_diff","arguments":{"path":"src","ref":"HEAD"}}),
            serde_json::json!({}),
        ];
        // Literal oracle from the old server representation, independent of the
        // new key constructor. In particular the server does NOT fold aliases.
        let server_old = [
            r#"read_file:{"limit":1,"path":"private-argument-sentinel"}"#,
            r#"read_file:{"limit":1,"path":"private-argument-sentinel"}"#,
            r#"read_file:{"limit":1,"path":"different"}"#,
            r#"bash:{"command":"git diff -- src/"}"#,
            r#"git_diff:{"path":"src","ref":"HEAD"}"#,
            ":",
        ];
        let server_new: Vec<_> = calls
            .iter()
            .map(|call| server_tool_call_signature(std::slice::from_ref(call)))
            .collect();
        let round_new: Vec<_> = calls
            .iter()
            .map(|call| round_tool_call_sig_and_names(std::slice::from_ref(call)).0)
            .collect();
        let round_old: Vec<_> = calls
            .iter()
            .map(|call| {
                crate::tool::result::semantics::tool_dedup_signature(
                    tool_call_name(call).unwrap_or(""),
                    &tool_call_arguments_value(call),
                )
            })
            .collect();
        for a in 0..calls.len() {
            for b in 0..calls.len() {
                assert_eq!(
                    server_old[a] == server_old[b],
                    server_new[a] == server_new[b]
                );
                assert_eq!(round_old[a] == round_old[b], round_new[a] == round_new[b]);
            }
        }
        assert!(
            !serde_json::to_string(&server_new)
                .unwrap()
                .contains("private-argument-sentinel")
        );
        assert!(
            !serde_json::to_string(&round_new)
                .unwrap()
                .contains("private-argument-sentinel")
        );
    }

    fn make_sigs(rounds: &[&[&str]]) -> Vec<BTreeSet<crate::stall::StallSignature>> {
        rounds
            .iter()
            .map(|tools| {
                tools
                    .iter()
                    .map(|t| StallSignature::new(t, b"{}"))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn trailing_identical_sig_depth_behavior() {
        // Counts streak from tail
        assert_eq!(trailing_identical_sig_depth(&[]), 0);
        assert_eq!(trailing_identical_sig_depth(&make_sigs(&[&["bash"]])), 1);
        assert_eq!(
            trailing_identical_sig_depth(&make_sigs(&[&["bash"], &["bash"]])),
            2
        );
        assert_eq!(
            trailing_identical_sig_depth(&make_sigs(&[
                &["bash"],
                &["bash"],
                &["bash"],
                &["bash"],
                &["bash"]
            ])),
            5
        );
        // Resets when last entry differs
        assert_eq!(
            trailing_identical_sig_depth(&make_sigs(&[&["bash"], &["bash"], &["bash"], &["git"]])),
            1
        );
        // Empty sig sets don't count (avoid double-counting round gaps)
        assert_eq!(
            trailing_identical_sig_depth(&[BTreeSet::new(), BTreeSet::new()]),
            0
        );
    }

    // ── Stall detection ──

    #[test]
    fn detect_server_stall_window() {
        // Below window: no stall
        assert!(!detect_server_stall(&make_sigs(&[&["bash"], &["bash"]]), 3).unwrap());
        // Exact repeats at window: stall
        assert!(detect_server_stall(&make_sigs(&[&["bash"], &["bash"], &["bash"]]), 3).unwrap());
        // Different tools: no stall
        assert!(
            !detect_server_stall(&make_sigs(&[&["bash"], &["read_file"], &["bash"]]), 3).unwrap()
        );
    }

    // ── CLI agentic: sig/name helpers + name-only stall ──

    #[test]
    fn round_tool_call_sig_and_names_records_exact_canonical_calls() {
        let c1 = vec![serde_json::json!({
            "id": "call_1", "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
        })];
        let (sigs, names) = round_tool_call_sig_and_names(&c1);
        assert_eq!(
            sigs,
            BTreeSet::from([StallSignature::new("read_file", br#"{"path":"a.rs"}"#)])
        );
        assert!(names.contains("read_file"));

        let c2 = vec![serde_json::json!({
            "id": "call_2", "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"b.rs\"}"}
        })];
        let (sigs, names) = round_tool_call_sig_and_names(&c2);
        assert_eq!(
            sigs,
            BTreeSet::from([StallSignature::new("read_file", br#"{"path":"b.rs"}"#)])
        );
        assert!(names.contains("read_file"));
    }

    #[test]
    fn round_tool_call_sig_canonicalizes_equivalent_shell_diff_commands() {
        let bash = vec![serde_json::json!({
            "id": "call_bash",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{\"command\":\"git diff -- src/\"}"
            }
        })];
        let structured = vec![serde_json::json!({
            "id": "call_git_diff",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{\"command\":\"git --no-pager diff -- src\"}"
            }
        })];

        let (bash_sigs, bash_names) = round_tool_call_sig_and_names(&bash);
        let (structured_sigs, structured_names) = round_tool_call_sig_and_names(&structured);

        assert_eq!(bash_sigs, structured_sigs);
        assert!(bash_names.contains("bash"));
        assert!(structured_names.contains("bash"));
    }

    // ── assess_progress (general progress-aware stall) ──

    fn sig_set(s: &[&str]) -> BTreeSet<crate::stall::StallSignature> {
        s.iter()
            .map(|x| {
                let (name, args) = x.split_once(':').unwrap_or((x, ""));
                StallSignature::new(name, args.as_bytes())
            })
            .collect()
    }

    #[test]
    fn assess_progress_healthy_when_insufficient_history() {
        let rounds = vec![sig_set(&["read_file:a"])];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::Healthy
        );
    }

    #[test]
    fn assess_progress_no_progress_on_exact_repeat() {
        let r = sig_set(&["bash:"]);
        let rounds = vec![r.clone(), r.clone(), r];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::NoProgress
        );
    }

    /// Regression: the real review-task pattern from session
    /// bc74b214-3e2e — three consecutive distinct `read_file` calls.
    /// Distinct reads must remain Healthy in the signature assessment;
    /// the detector does not establish task progress or authorize correction.
    #[test]
    fn assess_progress_healthy_on_distinct_reads() {
        let rounds = vec![
            sig_set(&["read_file:a"]),
            sig_set(&["read_file:b"]),
            sig_set(&["read_file:c"]),
        ];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::Healthy
        );
    }

    #[test]
    fn assess_progress_low_novelty_on_narrow_rotation() {
        // Two signatures alternating — 2 distinct / 6 total ≈ 0.33 < floor.
        let a = sig_set(&["bash:x", "read_file:y"]);
        let b = sig_set(&["bash:x", "read_file:y"]);
        let c = sig_set(&["bash:x", "read_file:y"]);
        // NOTE: these are IDENTICAL, so this actually hits NoProgress.
        let rounds = vec![a, b, c];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::NoProgress
        );
    }

    #[test]
    fn assess_progress_healthy_on_diverse_multi_tool_review() {
        let rounds = vec![
            sig_set(&["grep:pat1", "read_file:a"]),
            sig_set(&["grep:pat2", "read_file:b"]),
            sig_set(&["list_dir:/x", "read_file:c"]),
        ];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::Healthy
        );
    }

    #[test]
    fn assess_progress_invalid_window() {
        assert!(matches!(
            assess_progress(&[], 0),
            Err(StallDetectionError::InvalidWindowOrBudget(0))
        ));
    }

    /// Regression: `detect_divergence` must NOT flip to Diverging on the
    /// real review pattern (3 rounds of distinct read_file calls). This
    /// was the root cause of the false-positive "Stall correction
    /// injected" in session bc74b214-3e2e turn 2.
    #[test]
    fn detect_divergence_review_pattern_is_not_diverging() {
        let rounds = vec![
            sig_set(&["read_file:a"]),
            sig_set(&["read_file:b"]),
            sig_set(&["read_file:c"]),
        ];
        assert_eq!(
            detect_divergence(&rounds).unwrap(),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn detect_divergence_exact_repeat_is_diverging() {
        let r = sig_set(&["bash:"]);
        let rounds = vec![r.clone(), r.clone(), r];
        assert!(matches!(
            detect_divergence(&rounds).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    // ── Divergence detection ──

    #[test]
    fn divergence_healthy_empty() {
        assert_eq!(detect_divergence(&[]).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_healthy_productive() {
        let sigs = make_sigs(&[&["github"], &["memory"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    // ─── New progress-aware semantics ───────────────────────────────
    // Prior tests encoded the whitelist-based "3 exploration rounds =
    // diverging" heuristic. Under the new progress-aware judge, mixed
    // distinct-signature rounds are Healthy; only exact signature
    // repetition (genuine loops) promotes to Diverging.

    #[test]
    fn divergence_diverse_rounds_are_healthy() {
        // The old false-positive pattern: 3+ rounds of distinct exploration
        // tool calls. Under progress-aware detection these are Healthy
        // because each round contributes a new signature (distinct_sigs / total > floor).
        let sigs = make_sigs(&[&["bash"], &["list_dir"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);

        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["grep"],
            &["read_file"],
            &["glob"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);

        let sigs = make_sigs(&[
            &["bash", "grep"],
            &["list_dir", "read_file"],
            &["bash", "glob"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_exact_repeat_is_diverging() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        assert!(matches!(
            detect_divergence(&sigs).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn divergence_exact_multi_tool_repeat_is_diverging() {
        let sigs = make_sigs(&[
            &["bash", "read_file"],
            &["bash", "read_file"],
            &["bash", "read_file"],
        ]);
        assert!(matches!(
            detect_divergence(&sigs).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn divergence_productive_call_diverse_remains_healthy() {
        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["github"],
            &["bash"],
            &["list_dir"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_multi_tool_with_productive() {
        let sigs = make_sigs(&[&["bash", "memory"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    /// Regression for session bc74b214-3e2e turn-2 false positive:
    /// a normal code-analysis pattern (grep/read_file/grep/grep etc.)
    /// with differing tool *presence* per round must NOT flip to Diverging.
    /// The previous whitelist-based detector misfired here.
    #[test]
    fn normal_code_analysis_is_healthy() {
        let sigs = make_sigs(&[
            &["grep", "grep"],
            &["read_file"],
            &["grep"],
            &["grep", "grep"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    // ── Tool call signature format tests ──

    #[test]
    fn signature_nested_openai_format() {
        // OpenAI format: {function: {name, arguments}}
        let tool_calls = vec![serde_json::json!({
            "function": {
                "name": "read_file",
                "arguments": r#"{"path":"src/main.rs"}"#
            }
        })];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 1);
        let sig = sigs.iter().next().unwrap();
        assert_eq!(
            sig,
            &StallSignature::new("read_file", br#"{"path":"src/main.rs"}"#)
        );
    }

    #[test]
    fn signature_flat_internal_format() {
        // Internal flat format: {name, arguments}
        let tool_calls = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/main.rs"}
        })];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 1);
        let sig = sigs.iter().next().unwrap();
        assert_eq!(
            sig,
            &StallSignature::new("read_file", br#"{"path":"src/main.rs"}"#)
        );
    }

    #[test]
    fn signature_flat_format_different_tools_not_equal() {
        // Two different tool calls in flat format must produce different signatures
        let calls_a = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/main.rs"}
        })];
        let calls_b = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/lib.rs"}
        })];
        let sigs_a = server_tool_call_signature(&calls_a);
        let sigs_b = server_tool_call_signature(&calls_b);
        assert_ne!(
            sigs_a, sigs_b,
            "different paths must produce different signatures"
        );
    }

    #[test]
    fn signature_flat_format_different_tool_names() {
        let calls_a = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/main.rs"}
        })];
        let calls_b = vec![serde_json::json!({
            "name": "list_dir",
            "arguments": {"path": "src/"}
        })];
        let sigs_a = server_tool_call_signature(&calls_a);
        let sigs_b = server_tool_call_signature(&calls_b);
        assert_ne!(
            sigs_a, sigs_b,
            "different tool names must produce different signatures"
        );
    }

    /// Regression test for session 2c701822: flat-format tool calls were
    /// all producing signature ":" (empty name, empty args), causing false
    /// stall detection on EVERY round after the first.
    #[test]
    fn no_false_stall_with_flat_format_different_args() {
        // Simulate 4 rounds of read_file with different paths (flat format)
        let mut tool_sigs: Vec<BTreeSet<crate::stall::StallSignature>> = Vec::new();
        let window = SERVER_STALL_WINDOW.max(MAX_EXPLORATION_ROUNDS) + 2;

        // Round 1: read_file(Cargo.toml)
        let calls_1 = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "crates/astra/Cargo.toml"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls_1, window);
        assert!(!detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap());

        // Round 2: list_dir + read_file(different path)
        let calls_2 = vec![
            serde_json::json!({"name": "list_dir", "arguments": {"path": "crates/astra/src/edge_tools"}}),
            serde_json::json!({"name": "read_file", "arguments": {"path": "crates/astra/src/edge_tools/nonexistent.rs"}}),
        ];
        record_server_tool_signatures(&mut tool_sigs, &calls_2, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "different tool calls across rounds must not trigger stall"
        );

        // Round 3: read_file(yet another path)
        let calls_3 = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "crates/astra/src/edge_tools/mo_tools.rs"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls_3, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "read_file with different paths across rounds must not trigger stall"
        );

        // Round 4: str_replace (completely different tool)
        let calls_4 = vec![serde_json::json!({
            "name": "str_replace",
            "arguments": {"path": "Cargo.toml", "old_str": "foo", "new_str": "bar"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls_4, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "str_replace after read_file must not trigger stall"
        );
    }

    /// Verify that ACTUAL stall (same tool, same args) is still detected with flat format
    #[test]
    fn real_stall_detected_with_flat_format() {
        let mut tool_sigs: Vec<BTreeSet<crate::stall::StallSignature>> = Vec::new();
        let window = SERVER_STALL_WINDOW.max(MAX_EXPLORATION_ROUNDS) + 2;

        // Same exact call 3x in a row (SERVER_STALL_WINDOW=3)
        let calls = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/main.rs"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls, window);
        assert!(!detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap());

        record_server_tool_signatures(&mut tool_sigs, &calls, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "2 identical calls should not trigger stall with window=3"
        );

        record_server_tool_signatures(&mut tool_sigs, &calls, window);
        assert!(
            detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "3 identical tool calls across rounds must trigger stall"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    //  canonical_tool_args
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn canonical_tool_args_normalization() {
        // Normalizes whitespace and key ordering
        let raw = r#"{  "path" :  "src/main.rs" ,  "line": 42 }"#;
        assert_eq!(
            canonical_tool_args(raw),
            r#"{"line":42,"path":"src/main.rs"}"#
        );
        // Invalid JSON returns raw
        assert_eq!(canonical_tool_args("not json"), "not json");
        // Empty string passthrough
        assert_eq!(canonical_tool_args(""), "");
        // Nested objects/arrays
        assert_eq!(
            canonical_tool_args(r#"{"a": [1, 2, {"b": 3}]}"#),
            r#"{"a":[1,2,{"b":3}]}"#
        );
        // Key ordering normalized (two different-order inputs produce same output)
        assert_eq!(
            canonical_tool_args(r#"{"z": 1, "a": 2}"#),
            canonical_tool_args(r#"{"a": 2, "z": 1}"#)
        );
        // Plain string
        assert_eq!(canonical_tool_args(r#""hello""#), r#""hello""#);
        // Number
        assert_eq!(canonical_tool_args("42"), "42");
        // Empty object
        assert_eq!(canonical_tool_args("{}"), "{}");
    }

    // ══════════════════════════════════════════════════════════════════════
    //  server_tool_call_signature — edge cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn signature_empty_tool_calls() {
        let sigs = server_tool_call_signature(&[]);
        assert!(sigs.is_empty());
    }

    #[test]
    fn signature_multiple_tool_calls_returns_set() {
        let tool_calls = vec![
            serde_json::json!({"function": {"name": "bash", "arguments": r#"{"cmd":"ls"}"#}}),
            serde_json::json!({"function": {"name": "read_file", "arguments": r#"{"path":"a.rs"}"#}}),
        ];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 2);
    }

    #[test]
    fn signature_dedup_identical_tool_calls() {
        let tc = serde_json::json!({"function": {"name": "bash", "arguments": r#"{"cmd":"ls"}"#}});
        let sigs = server_tool_call_signature(&[tc.clone(), tc]);
        assert_eq!(sigs.len(), 1, "BTreeSet should dedup identical signatures");
    }

    #[test]
    fn signature_missing_name_field() {
        let tool_calls = vec![serde_json::json!({"function": {"arguments": r#"{"x":1}"#}})];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 1);
        let sig = sigs.iter().next().unwrap();
        assert_eq!(sig.tool_name(), "");
    }

    #[test]
    fn signature_missing_arguments_field() {
        let tool_calls = vec![serde_json::json!({"function": {"name": "bash"}})];
        let sigs = server_tool_call_signature(&tool_calls);
        let sig = sigs.iter().next().unwrap();
        assert_eq!(sig.tool_name(), "bash");
    }

    #[test]
    fn signature_completely_empty_object() {
        let tool_calls = vec![serde_json::json!({})];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 1);
        // Falls through to flat branch: empty name, empty args
        let sig = sigs.iter().next().unwrap();
        assert_eq!(sig.tool_name(), "");
    }

    // ══════════════════════════════════════════════════════════════════════
    //  record_server_tool_signatures
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn record_sigs_empty_tool_calls_preserves_history() {
        let mut sigs = vec![BTreeSet::from([StallSignature::new("bash", b"{}")])];
        record_server_tool_signatures(&mut sigs, &[], 5);
        assert_eq!(
            sigs.len(),
            1,
            "text-only turn (empty tool_calls) must preserve stall history"
        );
    }

    #[test]
    fn record_sigs_window_trims_oldest() {
        let mut sigs: Vec<BTreeSet<crate::stall::StallSignature>> = Vec::new();
        let calls = vec![serde_json::json!({"name": "bash", "arguments": {"cmd": "ls"}})];
        for _ in 0..5 {
            record_server_tool_signatures(&mut sigs, &calls, 3);
        }
        assert_eq!(sigs.len(), 3, "should trim to window size");
    }

    #[test]
    fn record_sigs_single_call() {
        let mut sigs: Vec<BTreeSet<crate::stall::StallSignature>> = Vec::new();
        let calls = vec![serde_json::json!({"name": "grep", "arguments": {"pattern": "foo"}})];
        record_server_tool_signatures(&mut sigs, &calls, 5);
        assert_eq!(sigs.len(), 1);
        assert!(sigs[0].iter().any(|s| s.tool_name() == "grep"));
    }

    #[test]
    fn record_sigs_exactly_at_window_no_trim() {
        let mut sigs: Vec<BTreeSet<crate::stall::StallSignature>> = Vec::new();
        let calls = vec![serde_json::json!({"name": "bash", "arguments": {}})];
        for _ in 0..3 {
            record_server_tool_signatures(&mut sigs, &calls, 3);
        }
        assert_eq!(sigs.len(), 3, "exactly at window should not over-trim");
    }

    // ══════════════════════════════════════════════════════════════════════
    //  detect_server_stall — additional cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn stall_empty_input() {
        assert!(!detect_server_stall(&[], 3).unwrap());
    }

    #[test]
    fn stall_detected_in_longer_history() {
        // Varied history followed by 3 identical → stall
        let sigs = make_sigs(&[&["grep"], &["list_dir"], &["bash"], &["bash"], &["bash"]]);
        assert!(detect_server_stall(&sigs, 3).unwrap());
    }

    #[test]
    fn stall_not_detected_when_last_entry_differs() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["grep"]]);
        assert!(!detect_server_stall(&sigs, 3).unwrap());
    }

    #[test]
    fn stall_multi_tool_identical_rounds() {
        // Multi-tool rounds that are identical
        let round: BTreeSet<crate::stall::StallSignature> = [
            StallSignature::new("bash", b"{}"),
            StallSignature::new("grep", b"{}"),
        ]
        .into_iter()
        .collect();
        let sigs = vec![round.clone(), round.clone(), round];
        assert!(detect_server_stall(&sigs, 3).unwrap());
    }

    #[test]
    fn stall_multi_tool_one_round_differs() {
        let round_a: BTreeSet<crate::stall::StallSignature> = [
            StallSignature::new("bash", b"{}"),
            StallSignature::new("grep", b"{}"),
        ]
        .into_iter()
        .collect();
        let round_b: BTreeSet<crate::stall::StallSignature> = [
            StallSignature::new("bash", b"{}"),
            StallSignature::new("list_dir", b"{}"),
        ]
        .into_iter()
        .collect();
        let sigs = vec![round_a.clone(), round_b, round_a];
        assert!(!detect_server_stall(&sigs, 3).unwrap());
    }

    // ══════════════════════════════════════════════════════════════════════
    //  detect_divergence_with_window — custom windows
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn divergence_with_budget_2_triggers_at_exact_repeat() {
        // New semantics: window=2, both rounds identical sig → Diverging.
        let sigs = make_sigs(&[&["bash"], &["bash"]]);
        assert!(matches!(
            detect_divergence_with_window(&sigs, 2).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn divergence_with_budget_2_distinct_rounds_healthy() {
        // New semantics: two distinct rounds within window=2 → Healthy
        // (novelty = 2/2 = 100%).
        let sigs = make_sigs(&[&["bash"], &["read_file"]]);
        assert_eq!(
            detect_divergence_with_window(&sigs, 2).unwrap(),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn divergence_with_budget_1_single_round_diverging() {
        // window=1 → a single round trivially equals itself → Diverging.
        let sigs = make_sigs(&[&["bash"]]);
        assert!(matches!(
            detect_divergence_with_window(&sigs, 1).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn divergence_with_budget_larger_than_history_is_healthy() {
        // Not enough history to judge → Healthy (new semantics).
        let sigs = make_sigs(&[&["bash"], &["read_file"]]);
        assert_eq!(
            detect_divergence_with_window(&sigs, 10).unwrap(),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn divergence_with_budget_empty_sigs() {
        assert_eq!(
            detect_divergence_with_window(&[], 5).unwrap(),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn divergence_with_budget_zero_is_error() {
        assert_eq!(
            detect_divergence_with_window(&[], 0),
            Err(StallDetectionError::InvalidWindowOrBudget(0))
        );
    }

    #[test]
    fn divergence_empty_sig_set_round_in_window_is_healthy() {
        // An empty sig set means the agent produced no tool calls that
        // round — can't judge progress from that. New semantics: Healthy.
        let mut sigs = make_sigs(&[&["bash"], &["read_file"]]);
        sigs.push(BTreeSet::new());
        sigs.extend(make_sigs(&[&["bash"]]));
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_mixed_productive_and_exploration_healthy() {
        // Any mix of distinct signatures → Healthy, regardless of which
        // tools are "productive" vs "exploratory" (no whitelist in new logic).
        let sigs = make_sigs(&[
            &["bash"],
            &["read_file"],
            &["write_file"],
            &["bash"],
            &["grep"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn server_stall_text_turn_does_not_clear_history() {
        let bash_ls = vec![serde_json::json!({
            "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}
        })];
        let window = 3;
        let mut sigs = Vec::new();

        // Turn 1: bash ls
        record_server_tool_signatures(&mut sigs, &bash_ls, window);
        assert_eq!(sigs.len(), 1);

        // Turn 2: bash ls
        record_server_tool_signatures(&mut sigs, &bash_ls, window);
        assert_eq!(sigs.len(), 2);

        // Turn 3: text-only (empty tool_calls) — must NOT clear history
        record_server_tool_signatures(&mut sigs, &[], window);
        assert_eq!(sigs.len(), 2, "text-only turn must not wipe stall history");

        // Turn 4: bash ls — this is the 3rd identical tool turn
        record_server_tool_signatures(&mut sigs, &bash_ls, window);
        assert_eq!(sigs.len(), 3);

        // Stall should be detected: 3 identical tool turns in window of 3
        let stalled = detect_server_stall(&sigs, window).unwrap();
        assert!(
            stalled,
            "stall must be detected despite interleaved text turn"
        );
    }
}

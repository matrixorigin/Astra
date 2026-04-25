//! Turn evaluation engine — computes success/quality from turn signals.
//!
//! Replaces the crude heuristic (tool_calls > 0 → success, 0.7 quality) with
//! a multi-signal evaluator that considers:
//! - Tool error rate (fraction of calls that failed)
//! - Empty/minimal tool output
//! - Stall/verdict events from TurnGuard
//! - Budget pressure
//! - Repeated tool calls (retry loops)
//! - Correction patterns in user follow-up

use astra_services::session_journal::{JournalEvent, ToolCallRecord};
use serde_json::json;

use crate::chat_turn_heuristics::looks_like_live_query_with_context;

/// Signals detected during evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalSignal {
    /// Fraction of tool calls that errored (0.0 = all ok, 1.0 = all failed).
    ToolErrorRate(f64),
    /// At least one tool returned empty or minimal output (<10 bytes).
    EmptyToolOutput,
    /// TurnGuard detected a stall (name stall, intent drift, etc.).
    StallDetected,
    /// Budget pressure exceeded 0.8 (agent struggling to fit in budget).
    HighBudgetPressure,
    /// Same tool/target called 3+ times — possible retry loop.
    RepeatToolCall(String),
    /// TurnGuard issued a warning or higher verdict.
    VerdictWarning,
    /// No tools used despite likely needing them.
    NoToolsNeeded,
    /// All tools succeeded with good output.
    AllToolsHealthy,
    /// A long run of consecutive single-tool rounds was detected — the model
    /// likely should have batched these into parallel rounds. Carries the
    /// length of the longest consecutive single-tool-round streak.
    SequentialReadChurn(usize),
    /// Multiple read tool calls hit overlapping line ranges of the same file
    /// without any intervening workspace mutation, suggesting the model
    /// re-read content it had already loaded into context. Carries the
    /// number of redundant read events (each read after the first overlap
    /// in a file's no-mutation window counts once). Calibrated against
    /// real session data; see `REDUNDANT_OVERLAPPING_READS_THRESHOLD`.
    RedundantOverlappingReads(usize),
}

/// Default threshold for [`EvalSignal::SequentialReadChurn`]: how many
/// consecutive single-tool rounds we tolerate before flagging the turn.
/// Calibrated against real session data: healthy turns (mutate→verify chains,
/// locate→read pairs) observed up to 6; wasted turns (pure exploratory churn)
/// observed at 10+.
pub const SEQUENTIAL_READ_CHURN_THRESHOLD: usize = 8;

/// Default threshold for [`EvalSignal::RedundantOverlappingReads`]: minimum
/// count of redundant read events needed before flagging the turn. Calibrated
/// against 14k real sessions: at ≥3, the signal flags ~15% of `rounds≥8`
/// turns and catches all confirmed-waste fixtures (c49bc4a3 t2 = 38,
/// eafda07e t2 = 19, 8ba9d165 t2 = 19, 4178c6a7 t2 = 19, bbf46ab2 t3 = 11,
/// bbf46ab2 t4 = 7) while leaving healthy short turns silent.
pub const REDUNDANT_OVERLAPPING_READS_THRESHOLD: usize = 3;

/// Post-mortem evaluation thresholds. Defaults mirror the calibrated compile-
/// time constants above, but runtime callers may override them from config so
/// passive eval signals can be tuned without a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationThresholds {
    pub sequential_read_churn: usize,
    pub redundant_overlapping_reads: usize,
}

impl Default for EvaluationThresholds {
    fn default() -> Self {
        Self {
            sequential_read_churn: SEQUENTIAL_READ_CHURN_THRESHOLD,
            redundant_overlapping_reads: REDUNDANT_OVERLAPPING_READS_THRESHOLD,
        }
    }
}

/// Result of evaluating a turn's success and quality.
#[derive(Debug, Clone)]
pub struct TurnEvaluation {
    /// Whether the turn is considered successful.
    pub success: bool,
    /// Quality score 0.0–1.0.
    pub quality: f64,
    /// How confident we are in this evaluation (0.0–1.0).
    pub confidence: f64,
    /// Signals that contributed to the evaluation.
    pub signals: Vec<EvalSignal>,
    /// Thresholds used when deriving configurable passive eval signals.
    pub thresholds: EvaluationThresholds,
}

/// Per-tool-call record for evaluation (matches ToolCallRecord shape).
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub name: String,
    pub repeat_key: String,
    pub ok: bool,
    pub ms: u64,
    pub error: Option<String>,
    pub output_bytes: Option<u32>,
}

/// Evaluate a completed turn from its observable signals.
///
/// # Arguments
/// - `tool_calls` — per-tool-call audit records
/// - `stall_count` — number of stall events in this turn
/// - `verdict_warning` — whether TurnGuard issued a Warning or higher
/// - `budget_pressure` — 0.0–0.9 budget pressure from compaction tier
/// - `is_factual_query` — whether the query likely needed tool calls
pub fn evaluate_turn(
    tool_calls: &[ToolCallInfo],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    is_factual_query: bool,
) -> TurnEvaluation {
    let mut signals = Vec::new();
    let mut quality = 0.5_f64; // base quality
    let mut confidence = 0.5_f64; // base confidence

    let total_calls = tool_calls.len();

    // ─── No tool calls ──────────────────────────────────────────────────
    if total_calls == 0 {
        if is_factual_query {
            // Needed tools but didn't use any — bad
            signals.push(EvalSignal::NoToolsNeeded);
            return TurnEvaluation {
                success: false,
                quality: 0.2,
                confidence: 0.7,
                signals,
                thresholds: EvaluationThresholds::default(),
            };
        }
        // Conversational turn — no tools expected
        return TurnEvaluation {
            success: true,
            quality: 0.5,
            confidence: 0.4, // low confidence for text-only turns
            signals,
            thresholds: EvaluationThresholds::default(),
        };
    }

    // ─── Tool error analysis ────────────────────────────────────────────
    let error_count = tool_calls.iter().filter(|tc| !tc.ok).count();
    let error_rate = error_count as f64 / total_calls as f64;
    signals.push(EvalSignal::ToolErrorRate(error_rate));

    if error_rate == 0.0 {
        // All tools succeeded
        quality += 0.3;
        confidence += 0.2;
        signals.push(EvalSignal::AllToolsHealthy);
    } else if error_rate < 0.5 {
        // Some errors but mostly ok
        quality += 0.1;
    } else {
        // Majority errors
        quality -= 0.2;
        confidence += 0.1; // more confident this is bad
    }

    // ─── Empty output detection ─────────────────────────────────────────
    let empty_outputs = tool_calls
        .iter()
        .filter(|tc| tc.ok && tc.output_bytes.is_some_and(|b| b < 10))
        .count();
    if empty_outputs > 0 {
        signals.push(EvalSignal::EmptyToolOutput);
        quality -= 0.1 * (empty_outputs as f64 / total_calls as f64);
    }

    // ─── Repeat tool detection (retry loops) ────────────────────────────
    let mut call_counts: std::collections::HashMap<&str, (&str, usize)> =
        std::collections::HashMap::new();
    for tc in tool_calls {
        let entry = call_counts
            .entry(tc.repeat_key.as_str())
            .or_insert((tc.name.as_str(), 0));
        entry.1 += 1;
    }
    for (name, count) in call_counts.values() {
        if *count >= 3 {
            signals.push(EvalSignal::RepeatToolCall((*name).to_string()));
            quality -= 0.15;
        }
    }

    // ─── Stall events ───────────────────────────────────────────────────
    if stall_count > 0 {
        signals.push(EvalSignal::StallDetected);
        quality -= 0.1 * stall_count.min(3) as f64;
        confidence += 0.1;
    }

    // ─── TurnGuard verdict ──────────────────────────────────────────────
    if verdict_warning {
        signals.push(EvalSignal::VerdictWarning);
        quality -= 0.15;
        confidence += 0.1;
    }

    // ─── Budget pressure ────────────────────────────────────────────────
    if budget_pressure > 0.8 {
        signals.push(EvalSignal::HighBudgetPressure);
        quality -= 0.05;
    }

    // ─── Determine success ──────────────────────────────────────────────
    let success = error_rate < 0.5 && quality > 0.3;

    TurnEvaluation {
        success,
        quality: quality.clamp(0.0, 1.0),
        confidence: confidence.clamp(0.0, 1.0),
        signals,
        thresholds: EvaluationThresholds::default(),
    }
}

pub fn evaluate_tool_call_records(
    input: &str,
    recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
) -> TurnEvaluation {
    evaluate_tool_call_records_with_thresholds(
        input,
        recent_tools,
        tool_call_records,
        stall_count,
        verdict_warning,
        budget_pressure,
        EvaluationThresholds::default(),
    )
}

pub fn evaluate_tool_call_records_with_thresholds(
    input: &str,
    recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    thresholds: EvaluationThresholds,
) -> TurnEvaluation {
    // Synthetic placeholders (skill skipped/deferred, surgically removed
    // parallel tool calls) are audit-only records that do NOT represent real
    // tool execution. Filtering them here is the single choke-point that
    // keeps tool_error_rate, RepeatToolCall, EmptyToolOutput, and the
    // success/quality verdict honest.
    let tool_calls = tool_call_records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder() && !record.was_blocked_by_policy())
        .map(|record| ToolCallInfo {
            name: record.name.clone(),
            // Prefer the *untruncated* args for the repeat-key. `args_preview`
            // is capped at ~80 chars, so two distinct calls that share a long
            // common prefix (e.g. `grep -n '<long-pattern>' /home/xupeng/githu…`)
            // collide and surface as a false repeat-loop. Hash `args_full`
            // when present to keep the key bounded; fall back to the preview
            // for legacy records, then to the bare tool name.
            repeat_key: record
                .args_full
                .as_deref()
                .map(str::trim)
                .filter(|full| !full.is_empty())
                .map(|full| {
                    // Hash is used only for in-process dedup within a single
                    // `evaluate_tool_call_records` call — never persisted or
                    // compared across Rust versions. DefaultHasher is fine here.
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    full.hash(&mut hasher);
                    format!("{}::full::{:016x}", record.name, hasher.finish())
                })
                .or_else(|| {
                    record
                        .args_preview
                        .as_deref()
                        .map(str::trim)
                        .filter(|preview| !preview.is_empty())
                        .map(|preview| format!("{}::preview::{preview}", record.name))
                })
                .unwrap_or_else(|| record.name.clone()),
            ok: record.ok,
            ms: record.ms,
            error: record.error.clone(),
            output_bytes: record.output_bytes,
        })
        .collect::<Vec<_>>();
    let is_live_query = looks_like_live_query_with_context(input, recent_tools);
    let mut eval = evaluate_turn(
        &tool_calls,
        stall_count,
        verdict_warning,
        budget_pressure,
        is_live_query,
    );
    eval.thresholds = thresholds;

    // ─── Sequential read-churn detection ────────────────────────────────
    // Count the longest run of consecutive single-tool rounds across the
    // real (non-synthetic, non-policy-blocked) records. Excludes records
    // without a `round` index (e.g., orphaned tail records). When the run
    // is ≥ the configured threshold, the model almost certainly
    // could have batched these calls into parallel rounds.
    let max_streak = longest_single_tool_round_streak(tool_call_records);
    if max_streak >= thresholds.sequential_read_churn {
        eval.signals
            .push(EvalSignal::SequentialReadChurn(max_streak));
        let penalty = (0.05 + 0.01 * (max_streak - thresholds.sequential_read_churn) as f64)
            .clamp(0.05, 0.20);
        eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
    }

    // ─── Redundant overlapping read detection ───────────────────────────
    // Detects the failure mode where the model re-reads the SAME file/line
    // range multiple times across a turn instead of referring back to its
    // own prior tool outputs. The detector excludes the legitimate
    // "mutate then re-read to verify" pattern: any workspace-mutating tool
    // call clears the per-file read history, so a re-read AFTER an edit
    // does NOT contribute to the count. Strictly observational at this
    // tier — no mid-loop intervention.
    let redundant_reads = count_redundant_overlapping_reads(tool_call_records);
    if redundant_reads >= thresholds.redundant_overlapping_reads {
        eval.signals
            .push(EvalSignal::RedundantOverlappingReads(redundant_reads));
        let penalty = (0.03
            + 0.01 * (redundant_reads - thresholds.redundant_overlapping_reads) as f64)
            .clamp(0.03, 0.15);
        eval.quality = (eval.quality - penalty).clamp(0.0, 1.0);
    }

    eval
}

/// Group real (non-synthetic, non-policy-blocked) tool-call records by their
/// `round` index and return the longest run of consecutive rounds that each
/// contained exactly one tool call. Records without a round index are
/// ignored to avoid false positives from orphaned tail entries.
fn longest_single_tool_round_streak(records: &[ToolCallRecord]) -> usize {
    use std::collections::BTreeMap;
    let mut per_round: BTreeMap<u32, usize> = BTreeMap::new();
    for record in records {
        if record.is_synthetic_placeholder() || record.was_blocked_by_policy() {
            continue;
        }
        if let Some(round) = record.round {
            *per_round.entry(round).or_insert(0) += 1;
        }
    }
    let mut current = 0_usize;
    let mut best = 0_usize;
    let mut prev_round: Option<u32> = None;
    for (&round, &count) in &per_round {
        // A gap in round indices breaks the streak — the missing round(s)
        // may have been filtered out (synthetic/blocked) and we cannot
        // assume they were single-tool.
        let adjacent = prev_round.is_none_or(|p| round == p + 1);
        if count == 1 && adjacent {
            current += 1;
            if current > best {
                best = current;
            }
        } else {
            current = if count == 1 { 1 } else { 0 };
        }
        prev_round = Some(round);
    }
    best
}

/// Parsed read target for redundancy detection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadRange {
    file: String,
    /// Line range, or `None` for whole-file reads.
    range: Option<(u32, u32)>,
}

fn ranges_overlap(a: &ReadRange, b: &ReadRange) -> bool {
    if a.file != b.file {
        return false;
    }
    match (a.range, b.range) {
        (None, _) | (_, None) => true,
        (Some((a0, a1)), Some((b0, b1))) => !(a1 < b0 || b1 < a0),
    }
}

/// Best-effort extraction of a read target from a tool-call's full args.
/// Returns `None` for tools that don't read file content (grep/ls/glob),
/// for ambiguous bash commands, and for parse failures. Recognized:
///   - `bash` with `sed -n '<a>,<b>p' <file>`
///   - `bash` with bare `cat <file>` (no shell redirection / pipe input)
///   - `view` tool with JSON args like `{"path":"<f>","view_range":[a,b]}`
fn extract_read_target(name: &str, args: &str) -> Option<ReadRange> {
    use regex::Regex;
    use std::sync::OnceLock;
    static SED_RANGE: OnceLock<Regex> = OnceLock::new();
    static CAT_FILE: OnceLock<Regex> = OnceLock::new();

    if name == "view" {
        // Prefer JSON parsing — `view` args_full is always JSON in practice.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args.trim()) {
            let path = v.get("path").and_then(|p| p.as_str())?.to_string();
            let range = v
                .get("view_range")
                .and_then(|r| r.as_array())
                .and_then(|arr| {
                    let s = arr.first()?.as_u64()? as u32;
                    let e = arr.get(1)?.as_u64()? as u32;
                    Some((s, e))
                });
            return Some(ReadRange { file: path, range });
        }
        return None;
    }
    if name != "bash" {
        return None;
    }
    // `sed -n 'A,Bp' file`  (with or without quotes around the range)
    let sed_re = SED_RANGE
        .get_or_init(|| Regex::new(r#"\bsed\s+-n\s+['"]?(\d+),(\d+)p['"]?\s+(\S+)"#).unwrap());
    if let Some(c) = sed_re.captures(args) {
        let s: u32 = c.get(1)?.as_str().parse().ok()?;
        let e: u32 = c.get(2)?.as_str().parse().ok()?;
        let f = c
            .get(3)?
            .as_str()
            .trim_matches(|x| x == '\'' || x == '"')
            .to_string();
        return Some(ReadRange {
            file: f,
            range: Some((s, e)),
        });
    }
    // Bare `cat <file>` — must NOT be part of a pipeline that accepts
    // input (heredoc, `<`, `<<`) and must NOT be a mutation (`>`/`>>`/`tee`).
    if args.contains('>') || args.contains("<<") {
        return None;
    }
    let cat_re = CAT_FILE.get_or_init(|| Regex::new(r#"\bcat\s+(\S+)"#).unwrap());
    if let Some(c) = cat_re.captures(args)
        && let Some(f) = c.get(1)
    {
        let f = f
            .as_str()
            .trim_matches(|x| x == '\'' || x == '"')
            .to_string();
        // Skip if the "file" looks like an option (e.g. `-n`).
        if !f.starts_with('-') {
            return Some(ReadRange {
                file: f,
                range: None,
            });
        }
    }
    None
}

/// Whether a tool call mutates workspace state in a way that justifies
/// a re-read of the same content. Conservative — any plausible mutation
/// clears the per-file read history so we don't over-flag legitimate
/// "edit then verify" patterns.
fn is_mutation_for_redundant_read(name: &str, args: &str) -> bool {
    matches!(name, "edit" | "create" | "write") || (name == "bash" && bash_args_look_mutating(args))
}

fn bash_args_look_mutating(args: &str) -> bool {
    // Output redirection or in-place mutation.
    if args.contains(">") || args.contains("|& tee") || args.contains("| tee") {
        return true;
    }
    // Common mutating commands as substrings (tolerates prefixes like
    // `cd /tmp && mv …` or `sudo rm …`).
    const MUT_NEEDLES: &[&str] = &[
        " mv ",
        " cp ",
        " rm ",
        "sed -i",
        "rmdir ",
        "mkdir ",
        "touch ",
        "chmod ",
        "chown ",
        "git commit",
        "git add",
        "git checkout",
        "git reset",
        "git rebase",
        "git merge",
        "git apply",
    ];
    let padded = format!(" {args} ");
    MUT_NEEDLES.iter().any(|n| padded.contains(n))
}

/// Best-effort extraction of the file targeted by a mutation, used to scope
/// per-file history clears. Returns `None` if the target file is unclear,
/// in which case the caller clears ALL per-file histories (conservative).
fn mutation_target_file(name: &str, args: &str) -> Option<String> {
    if matches!(name, "edit" | "create" | "write") {
        // Astra's edit/create tools take JSON args with a `path` field.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args.trim()) {
            return v.get("path").and_then(|p| p.as_str()).map(String::from);
        }
    }
    None
}

/// Count redundant overlapping reads — see `EvalSignal::RedundantOverlappingReads`.
///
/// Public so the runtime can call it mid-loop for a corrective intervention,
/// not just post-mortem. The runtime uses a slightly higher threshold than
/// the eval-signal threshold to err on the side of underkill for the
/// behavioral intervention.
pub fn count_redundant_overlapping_reads(records: &[ToolCallRecord]) -> usize {
    use std::collections::HashMap;
    let mut per_file: HashMap<String, Vec<ReadRange>> = HashMap::new();
    let mut redundant = 0usize;
    for rec in records {
        if rec.is_synthetic_placeholder() || rec.was_blocked_by_policy() {
            continue;
        }
        let args = rec.args_full.as_deref().unwrap_or("");
        if is_mutation_for_redundant_read(&rec.name, args) {
            // Mutation: invalidate the relevant file's read history (or all
            // file histories if we can't pinpoint the target).
            match mutation_target_file(&rec.name, args) {
                Some(f) => {
                    per_file.remove(&f);
                }
                None => per_file.clear(),
            }
            continue;
        }
        if let Some(target) = extract_read_target(&rec.name, args) {
            let entry = per_file.entry(target.file.clone()).or_default();
            if entry.iter().any(|prev| ranges_overlap(prev, &target)) {
                redundant += 1;
            }
            entry.push(target);
        }
    }
    redundant
}

pub fn eval_signal_to_json(signal: &EvalSignal) -> serde_json::Value {
    eval_signal_to_json_with_thresholds(signal, EvaluationThresholds::default())
}

pub fn eval_signal_to_json_with_thresholds(
    signal: &EvalSignal,
    thresholds: EvaluationThresholds,
) -> serde_json::Value {
    match signal {
        EvalSignal::ToolErrorRate(rate) => json!({
            "kind": "tool_error_rate",
            "weight": rate,
        }),
        EvalSignal::EmptyToolOutput => json!({
            "kind": "empty_tool_output",
            "message": "At least one successful tool call returned minimal output",
        }),
        EvalSignal::StallDetected => json!({
            "kind": "stall_detected",
            "message": "TurnGuard recorded a stall or divergence event",
        }),
        EvalSignal::HighBudgetPressure => json!({
            "kind": "high_budget_pressure",
            "message": "Budget pressure crossed the high-pressure threshold",
        }),
        EvalSignal::RepeatToolCall(name) => json!({
            "kind": "repeat_tool_call",
            "tool": name,
            "message": format!("Repeated tool call pattern detected for `{name}`"),
        }),
        EvalSignal::VerdictWarning => json!({
            "kind": "verdict_warning",
            "message": "TurnGuard emitted a warning-or-higher verdict",
        }),
        EvalSignal::NoToolsNeeded => json!({
            "kind": "missing_needed_tools",
            "message": "The turn looked like a live query but completed without tool calls",
        }),
        EvalSignal::AllToolsHealthy => json!({
            "kind": "all_tools_healthy",
            "message": "All tool calls completed successfully with non-empty output",
        }),
        EvalSignal::SequentialReadChurn(streak) => json!({
            "kind": "sequential_read_churn",
            "streak": streak,
            "threshold": thresholds.sequential_read_churn,
            "message": format!(
                "Detected {streak} consecutive single-tool rounds — these calls likely should have been batched into parallel rounds"
            ),
        }),
        EvalSignal::RedundantOverlappingReads(count) => json!({
            "kind": "redundant_overlapping_reads",
            "count": count,
            "threshold": thresholds.redundant_overlapping_reads,
            "message": format!(
                "Detected {count} redundant read(s) with no intervening workspace mutation — the model likely re-read content already in context"
            ),
        }),
    }
}

pub fn eval_signals_to_json(signals: &[EvalSignal]) -> Vec<serde_json::Value> {
    eval_signals_to_json_with_thresholds(signals, EvaluationThresholds::default())
}

pub fn eval_signals_to_json_with_thresholds(
    signals: &[EvalSignal],
    thresholds: EvaluationThresholds,
) -> Vec<serde_json::Value> {
    signals
        .iter()
        .map(|signal| eval_signal_to_json_with_thresholds(signal, thresholds))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn build_turn_evaluation_journal_event(
    session_id: Option<&str>,
    turn: Option<u32>,
    source: &str,
    input: &str,
    recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    eval: &TurnEvaluation,
) -> JournalEvent {
    JournalEvent::turn_evaluation(
        session_id,
        turn,
        source,
        looks_like_live_query_with_context(input, recent_tools),
        eval.success,
        eval.quality,
        eval.confidence,
        budget_pressure,
        stall_count,
        verdict_warning,
        // Exclude synthetic placeholders (surgical removals, skipped skills) from
        // the user-visible tool_call_count — they are audit-only records.
        tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder() && !r.was_blocked_by_policy())
            .count(),
        eval_signals_to_json_with_thresholds(&eval.signals, eval.thresholds),
    )
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::JournalEventType;

    fn ok_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            output_bytes: Some(500),
        }
    }

    fn err_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: false,
            ms: 50,
            error: Some("tool error".to_string()),
            output_bytes: None,
        }
    }

    fn empty_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            output_bytes: Some(0),
        }
    }

    fn journal_ok_call(name: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: None,
            result_preview: Some("ok".to_string()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn all_tools_succeed_high_quality() {
        let calls = vec![ok_call("bash"), ok_call("grep"), ok_call("read_file")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(eval.success);
        assert!(eval.quality > 0.7, "quality={}", eval.quality);
        assert!(eval.signals.contains(&EvalSignal::AllToolsHealthy));
    }

    #[test]
    fn all_tools_fail_low_quality() {
        let calls = vec![err_call("bash"), err_call("grep")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(!eval.success);
        assert!(eval.quality < 0.4, "quality={}", eval.quality);
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::ToolErrorRate(r) if *r > 0.9))
        );
    }

    #[test]
    fn mixed_success_moderate_quality() {
        let calls = vec![ok_call("bash"), err_call("grep"), ok_call("read_file")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(eval.success); // error rate < 0.5
        let rate_signal = eval
            .signals
            .iter()
            .find(|s| matches!(s, EvalSignal::ToolErrorRate(_)));
        assert!(rate_signal.is_some());
    }

    #[test]
    fn no_tools_conversational_ok() {
        let eval = evaluate_turn(&[], 0, false, 0.3, false);
        assert!(eval.success);
        assert_eq!(eval.quality, 0.5);
        assert!(eval.confidence < 0.5); // low confidence for text-only
    }

    #[test]
    fn no_tools_factual_query_bad() {
        let eval = evaluate_turn(&[], 0, false, 0.3, true);
        assert!(!eval.success);
        assert!(eval.quality < 0.3);
        assert!(eval.signals.contains(&EvalSignal::NoToolsNeeded));
    }

    #[test]
    fn stalls_reduce_quality() {
        let calls = vec![ok_call("bash")];
        let no_stall = evaluate_turn(&calls, 0, false, 0.3, false);
        let with_stall = evaluate_turn(&calls, 2, false, 0.3, false);
        assert!(with_stall.quality < no_stall.quality);
        assert!(with_stall.signals.contains(&EvalSignal::StallDetected));
    }

    #[test]
    fn verdict_warning_reduces_quality() {
        let calls = vec![ok_call("bash")];
        let no_verdict = evaluate_turn(&calls, 0, false, 0.3, false);
        let with_verdict = evaluate_turn(&calls, 0, true, 0.3, false);
        assert!(with_verdict.quality < no_verdict.quality);
        assert!(with_verdict.signals.contains(&EvalSignal::VerdictWarning));
    }

    #[test]
    fn high_budget_pressure_penalizes() {
        let calls = vec![ok_call("bash")];
        let low_pressure = evaluate_turn(&calls, 0, false, 0.3, false);
        let high_pressure = evaluate_turn(&calls, 0, false, 0.85, false);
        assert!(high_pressure.quality < low_pressure.quality);
        assert!(
            high_pressure
                .signals
                .contains(&EvalSignal::HighBudgetPressure)
        );
    }

    #[test]
    fn repeat_tool_calls_penalize() {
        let calls = vec![
            ok_call("bash"),
            ok_call("bash"),
            ok_call("bash"),
            ok_call("grep"),
        ];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(n) if n == "bash"))
        );
    }

    #[test]
    fn empty_output_penalizes() {
        let calls = vec![empty_call("read_file"), ok_call("bash")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(eval.signals.contains(&EvalSignal::EmptyToolOutput));
        let all_ok = evaluate_turn(
            &[ok_call("read_file"), ok_call("bash")],
            0,
            false,
            0.3,
            false,
        );
        assert!(eval.quality < all_ok.quality);
    }

    #[test]
    fn quality_clamped_to_bounds() {
        // Worst case: all errors + stalls + verdict + pressure
        let calls = vec![err_call("a"), err_call("b"), err_call("c")];
        let eval = evaluate_turn(&calls, 5, true, 0.9, true);
        assert!(eval.quality >= 0.0);
        assert!(eval.quality <= 1.0);
        assert!(eval.confidence >= 0.0);
        assert!(eval.confidence <= 1.0);
    }

    #[test]
    fn confidence_increases_with_more_signals() {
        let simple = evaluate_turn(&[ok_call("bash")], 0, false, 0.3, false);
        let complex = evaluate_turn(&[err_call("bash"), err_call("grep")], 2, true, 0.9, false);
        assert!(complex.confidence > simple.confidence);
    }

    #[test]
    fn evaluate_tool_call_records_reuses_live_query_heuristic() {
        let eval = evaluate_tool_call_records(
            "Check the latest git status",
            &["git_status".to_string()],
            &[],
            0,
            false,
            0.2,
        );
        assert!(!eval.success);
        assert!(eval.signals.contains(&EvalSignal::NoToolsNeeded));
    }

    #[test]
    fn build_turn_evaluation_journal_event_serializes_normalized_signals() {
        let records = vec![journal_ok_call("git_status")];
        let eval = evaluate_tool_call_records(
            "Check the latest git status",
            &["git_status".to_string()],
            &records,
            0,
            false,
            0.2,
        );

        let event = build_turn_evaluation_journal_event(
            Some("sess-1"),
            Some(2),
            "cli_repl",
            "Check the latest git status",
            &["git_status".to_string()],
            &records,
            0,
            false,
            0.2,
            &eval,
        );

        assert_eq!(event.event_type, JournalEventType::TurnEvaluation);
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["source"], "cli_repl");
        assert_eq!(metadata["live_query"], true);
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["signal_count"], 2);
        assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
        assert_eq!(metadata["signals"][1]["kind"], "all_tools_healthy");
    }

    #[test]
    fn evaluate_records_skips_synthetic_placeholders() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        let rec = |name: &str, ok: bool, preview: Option<&str>| ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: preview.map(ToString::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        // 1 real successful tool + 3 synthetic placeholders (skipped,
        // deferred, surgically_removed). A naive counter would see
        // error_rate = 3/4 = 0.75 and quality collapse. With filtering,
        // only the real success is counted and the turn scores as healthy.
        let records = vec![
            rec("git_show", true, Some("diff contents here")),
            rec(
                SURGICAL_REMOVAL_TOOL_NAME,
                true,
                Some("(removed from context — skill covered this work)"),
            ),
            rec("read_file", false, Some("Skipped: skill routed")),
            rec("read_file", false, Some("Deferred: skill invoked")),
        ];

        let eval = evaluate_tool_call_records(
            "review commit 179afcb",
            &["git_show".to_string()],
            &records,
            0,
            false,
            0.1,
        );

        // Must contain exactly one error_rate=0.0 signal AND AllToolsHealthy
        // — proves surgical/skipped/deferred were filtered before analysis.
        let has_zero_error = eval.signals.iter().any(
            |s| matches!(s, EvalSignal::ToolErrorRate(rate) if (*rate - 0.0).abs() < f64::EPSILON),
        );
        assert!(
            has_zero_error,
            "expected ToolErrorRate(0.0) after filtering synthetic placeholders, got {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::AllToolsHealthy)),
            "expected AllToolsHealthy signal, got {:?}",
            eval.signals
        );
        assert!(
            eval.success,
            "turn with one real success and synthetic placeholders must be success=true"
        );
        assert!(
            eval.quality > 0.6,
            "quality should be high after filtering, got {}",
            eval.quality
        );
    }

    #[test]
    fn repeat_tool_call_ignores_surgical_removals() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // Four surgically_removed records in a single turn used to surface
        // as RepeatToolCall("(surgically_removed)") — a false "retry loop"
        // signal. After filtering they must not appear at all.
        let records: Vec<ToolCallRecord> = (0..4)
            .map(|_| ToolCallRecord {
                name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
                ok: true,
                ms: 0,
                error: None,
                input_bytes: None,
                output_bytes: Some(0),
                args_preview: None,
                result_preview: Some("(removed from context — skill covered this work)".into()),
                file_path: None,
                surgically_removed: Some(true),
                original_tool_name: Some("read_file".to_string()),
                ..Default::default()
            })
            .chain(std::iter::once(ToolCallRecord {
                name: "git_show".to_string(),
                ok: true,
                ms: 20,
                error: None,
                input_bytes: None,
                output_bytes: Some(400),
                args_preview: None,
                result_preview: Some("diff".into()),
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            }))
            .collect();

        let eval = evaluate_tool_call_records("noop", &[], &records, 0, false, 0.1);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(_))),
            "no RepeatToolCall signal should be emitted for synthetic placeholders, got {:?}",
            eval.signals
        );
    }

    #[test]
    fn repeat_tool_call_distinguishes_distinct_args_preview() {
        use astra_services::session_journal::ToolCallRecord;

        let record = |name: &str, args_preview: &str| ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 20,
            error: None,
            input_bytes: None,
            output_bytes: Some(120),
            args_preview: Some(args_preview.to_string()),
            result_preview: Some("ok".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        let distinct_greps = vec![
            record("grep", "/catch_tool_execution_panic/ in ."),
            record("grep", "/CLOUD_APPROVAL_REQUIRED_TOOLS/ in ."),
            record(
                "grep",
                "/struct ToolExecutionOutcome/ in rust/crates/astra-cli/src",
            ),
            record("grep", "/struct ToolExecutionOutcome/ in ."),
        ];
        let eval = evaluate_tool_call_records(
            "review latest commit",
            &["grep".to_string()],
            &distinct_greps,
            0,
            false,
            0.1,
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "grep")),
            "distinct grep queries should not be treated as a retry loop: {:?}",
            eval.signals
        );

        let repeated_git_show = vec![
            record("git_show", "6f2f96e"),
            record("git_show", "6f2f96e"),
            record("git_show", "6f2f96e"),
        ];
        let eval = evaluate_tool_call_records(
            "review latest commit",
            &["git_show".to_string()],
            &repeated_git_show,
            0,
            false,
            0.1,
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "git_show")),
            "identical git_show targets should still surface as repeat loops: {:?}",
            eval.signals
        );
    }

    #[test]
    fn repeat_tool_call_uses_args_full_to_avoid_truncation_collisions() {
        // Reproduces a real false-positive observed in session
        // 24234a1f-1c01-4577-a06a-168e7b583c6d: three legitimate `grep` calls
        // against three distinct files all share the same ~80-char truncated
        // `args_preview` because the long pattern + common path prefix fills
        // the preview before the file name diverges. The repeat-loop signal
        // must look at the *untruncated* `args_full` so distinct calls never
        // collide just because their previews happen to share a prefix.
        use astra_services::session_journal::ToolCallRecord;

        let identical_truncated_preview =
            "grep -n 'canonicalize|unique_path_variants|normalize_path' /home/xupeng/githu…";

        let record = |full_path: &str| ToolCallRecord {
            name: "bash".to_string(),
            ok: true,
            ms: 51,
            error: None,
            input_bytes: None,
            output_bytes: Some(500),
            args_preview: Some(identical_truncated_preview.to_string()),
            args_full: Some(format!(
                r#"{{"command":"grep -n 'canonicalize|unique_path_variants|normalize_path' {}"}}"#,
                full_path
            )),
            result_preview: Some("ok".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        let distinct_full_args = vec![
            record("/home/xupeng/github/astra/rust/crates/services/src/durable_task.rs"),
            record("/home/xupeng/github/astra/rust/crates/astra-tools/src/fs_ops.rs"),
            record("/home/xupeng/github/astra/rust/crates/astra-sandbox/src/policy.rs"),
        ];

        let eval = evaluate_tool_call_records(
            "review canonicalize hot path",
            &["bash".to_string()],
            &distinct_full_args,
            0,
            false,
            0.15,
        );

        assert!(
            !eval.signals.iter().any(|s| matches!(
                s,
                EvalSignal::RepeatToolCall(name) if name == "bash"
            )),
            "three distinct bash invocations differing only past the args_preview \
             truncation must NOT surface as a retry loop: {:?}",
            eval.signals
        );

        // Sanity check: when args_full is genuinely identical, the signal
        // still fires (we did not break legitimate repeat-loop detection).
        let repeated = vec![
            ToolCallRecord {
                name: "bash".into(),
                ok: true,
                ms: 51,
                args_preview: Some(identical_truncated_preview.into()),
                args_full: Some(r#"{"command":"grep -n 'foo' bar.rs"}"#.into()),
                result_preview: Some("ok".into()),
                output_bytes: Some(120),
                ..Default::default()
            };
            3
        ];
        let eval =
            evaluate_tool_call_records("look", &["bash".to_string()], &repeated, 0, false, 0.1);
        assert!(
            eval.signals.iter().any(|s| matches!(
                s,
                EvalSignal::RepeatToolCall(name) if name == "bash"
            )),
            "identical args_full repeats must still surface as a retry loop: {:?}",
            eval.signals
        );
    }

    #[test]
    fn real_session_0ac769_pattern_surfaces_git_show_and_read_file_repeats() {
        use astra_services::session_journal::ToolCallRecord;

        let record = |name: &str, args_preview: &str| ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 20,
            error: None,
            input_bytes: None,
            output_bytes: Some(120),
            args_preview: Some(args_preview.to_string()),
            result_preview: Some("ok".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        // Real session 0ac7696c had 7 LLM rounds with this 12-tool pattern:
        // git_show, git_show, read_file x3 + grep, grep x3 + read_file, git_show, git_show.
        // The persisted turn_evaluation surfaced repeat loops for read_file and
        // git_show, while distinct grep queries stayed healthy.
        let records = vec![
            record(
                "git_show",
                r#"{"rev":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "git_show",
                r#"{"rev":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"rust/crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"rust/crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"rust/crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record("grep", r#"/factual retry/ in rust/crates/runtime/src"#),
            record("grep", r#"/ContinueLoop/ in rust/crates/runtime/src"#),
            record("grep", r#"/TPM/ in rust/crates/runtime/src"#),
            record(
                "read_file",
                r#"{"path":"rust/crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "git_show",
                r#"{"rev":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "git_show",
                r#"{"rev":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record("grep", r#"/turn_evaluation/ in rust/crates/runtime/src"#),
        ];

        let eval = evaluate_tool_call_records(
            "review b273c589a73799070a71f4cfc6d55349b534d8d1",
            &[
                "git_show".to_string(),
                "read_file".to_string(),
                "grep".to_string(),
            ],
            &records,
            0,
            false,
            0.446_225,
        );

        assert!(
            eval.success,
            "real-session loop still completed successfully"
        );
        assert_eq!(eval.quality, 0.5);
        assert_eq!(eval.confidence, 0.7);
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "read_file")),
            "expected read_file repeat signal, got {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "git_show")),
            "expected git_show repeat signal, got {:?}",
            eval.signals
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "grep")),
            "distinct grep queries should not collapse into a repeat loop: {:?}",
            eval.signals
        );

        let event = build_turn_evaluation_journal_event(
            Some("0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0"),
            Some(1),
            "cli_repl",
            "review b273c589a73799070a71f4cfc6d55349b534d8d1",
            &[
                "git_show".to_string(),
                "read_file".to_string(),
                "grep".to_string(),
            ],
            &records,
            0,
            false,
            0.446_225,
            &eval,
        );
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["tool_call_count"], 12);
        assert_eq!(metadata["signal_count"], 4);
        assert_eq!(metadata["quality"], 0.5);
        assert_eq!(metadata["confidence"], 0.7);
    }

    #[test]
    fn tool_call_count_excludes_synthetic_placeholders() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // 3 real tool calls + 2 surgical removals = 5 records total,
        // but tool_call_count should be 3 (only real).
        let real = || ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: Some("content".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        let surgical = || ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: Some(0),
            args_preview: None,
            result_preview: Some("(removed)".into()),
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("glob".to_string()),
            ..Default::default()
        };
        let records = vec![real(), surgical(), real(), surgical(), real()];

        let event = build_turn_evaluation_journal_event(
            Some("sess-1"),
            Some(1),
            "test-model",
            "do something",
            &[],
            &records,
            0,
            false,
            0.5,
            &TurnEvaluation {
                success: true,
                quality: 0.8,
                confidence: 0.9,
                signals: vec![],
                thresholds: EvaluationThresholds::default(),
            },
        );

        // Extract tool_call_count from the metadata JSON.
        let metadata = event.metadata.as_ref().expect("metadata should be present");
        let tool_call_count = metadata["tool_call_count"].as_u64().unwrap();
        assert_eq!(
            tool_call_count, 3,
            "tool_call_count must exclude synthetic placeholders, got {}",
            tool_call_count
        );
    }

    #[test]
    fn is_synthetic_placeholder_via_flag() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // New-style: surgically_removed flag set
        let flagged = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("bash".to_string()),
            ..Default::default()
        };
        assert!(flagged.is_synthetic_placeholder());

        // Backward-compat: legacy sentinel name only (no flag)
        let legacy = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        assert!(legacy.is_synthetic_placeholder());

        // Normal tool call: neither flag nor sentinel name
        let normal = ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        assert!(!normal.is_synthetic_placeholder());
    }

    // ─── Sequential read-churn (real-session-shaped fixtures) ───────────
    //
    // Real sessions inspected to calibrate the threshold:
    //   - 6566d6a8 turn 1: 10 consecutive single-tool read rounds → wasted
    //   - bbae8641 turn 3: 11 consecutive single-tool read rounds → wasted
    //   - 6da9cf8f turn 6: 16 consecutive single-tool read rounds → wasted
    //   - 03945541 turn 1: 6 single-tool rounds (locate→read chain) → healthy
    //   - 03945541 turn 2: max 4 single-tool rounds (mutate→verify) → healthy
    // Threshold of 8 cleanly separates these populations.

    fn record_in_round(name: &str, round: u32, batch: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            batch_id: batch.map(str::to_string),
            parallel: Some(batch.is_some()),
            round: Some(round),
            args_full: None,
            ..Default::default()
        }
    }

    #[test]
    fn sequential_read_churn_flags_long_single_tool_streak() {
        // Mirrors session 6566d6a8 turn 1: 10 consecutive read_file rounds,
        // each with exactly one tool call. The model should have batched.
        let mut records = Vec::new();
        for r in 0..10 {
            records.push(record_in_round("read_file", r, None));
        }
        let eval =
            evaluate_tool_call_records("explain the auth flow", &[], &records, 0, false, 0.3);
        let streak = eval.signals.iter().find_map(|s| match s {
            EvalSignal::SequentialReadChurn(n) => Some(*n),
            _ => None,
        });
        assert_eq!(
            streak,
            Some(10),
            "expected SequentialReadChurn(10), got signals={:?}",
            eval.signals
        );
        // Quality should be docked by the churn penalty.
        let baseline =
            evaluate_tool_call_records("explain the auth flow", &[], &records[..1], 0, false, 0.3);
        assert!(
            eval.quality < baseline.quality,
            "churn turn quality={} should be below baseline={}",
            eval.quality,
            baseline.quality
        );
    }

    #[test]
    fn sequential_read_churn_does_not_flag_well_batched_turn() {
        // Mirrors session 03945541 turn 2: 6 rounds, mostly batched in
        // parallel pairs, with a few legitimately-sequential rounds for
        // mutate→verify chains. max_consec_single_tool_rounds = 4.
        let records = vec![
            // round 0: 4-tool parallel batch
            record_in_round("git_show", 0, Some("b-0-0")),
            record_in_round("git_show", 0, Some("b-0-0")),
            record_in_round("git_show", 0, Some("b-0-0")),
            record_in_round("git_show", 0, Some("b-0-0")),
            // round 1: 2-tool parallel
            record_in_round("bash", 1, Some("b-1-0")),
            record_in_round("bash", 1, Some("b-1-0")),
            // rounds 2-5: 4 consecutive single-tool rounds (mutate→verify)
            record_in_round("str_replace", 2, None),
            record_in_round("read_file", 3, None),
            record_in_round("str_replace", 4, None),
            record_in_round("bash", 5, None),
            // round 6: 2-tool parallel batch (cargo test pair)
            record_in_round("bash", 6, Some("b-6-0")),
            record_in_round("bash", 6, Some("b-6-0")),
        ];
        let eval = evaluate_tool_call_records("ship the fix", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::SequentialReadChurn(_))),
            "well-batched turn should NOT emit SequentialReadChurn; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn sequential_read_churn_does_not_flag_parallel_only_turn() {
        // 12 rounds, every one with 3 parallel tools. Zero single-tool rounds.
        let mut records = Vec::new();
        for r in 0..12 {
            let batch = format!("b-{r}-0");
            for _ in 0..3 {
                records.push(record_in_round("read_file", r, Some(batch.as_str())));
            }
        }
        let eval = evaluate_tool_call_records("survey the codebase", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::SequentialReadChurn(_))),
            "all-parallel turn should NOT emit SequentialReadChurn; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn sequential_read_churn_below_threshold_is_silent() {
        // Mirrors 03945541 turn 1: 6 single-tool rounds, just under the
        // threshold of 8. Should NOT trigger.
        let records: Vec<_> = (0..6)
            .map(|r| record_in_round("git_show", r, None))
            .collect();
        let eval = evaluate_tool_call_records("review", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::SequentialReadChurn(_))),
            "6 single-tool rounds is below threshold; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn sequential_read_churn_signal_serializes_to_json() {
        let value = eval_signal_to_json(&EvalSignal::SequentialReadChurn(11));
        assert_eq!(value["kind"], "sequential_read_churn");
        assert_eq!(value["streak"], 11);
        assert_eq!(value["threshold"], SEQUENTIAL_READ_CHURN_THRESHOLD as i64);
        assert!(value["message"].as_str().unwrap().contains("11"));
    }

    #[test]
    fn sequential_read_churn_custom_threshold_is_respected() {
        let records: Vec<_> = (0..6)
            .map(|r| record_in_round("git_show", r, None))
            .collect();
        let eval = evaluate_tool_call_records_with_thresholds(
            "review",
            &[],
            &records,
            0,
            false,
            0.3,
            EvaluationThresholds {
                sequential_read_churn: 6,
                ..Default::default()
            },
        );
        let streak = eval.signals.iter().find_map(|s| match s {
            EvalSignal::SequentialReadChurn(n) => Some(*n),
            _ => None,
        });
        assert_eq!(streak, Some(6));
        assert_eq!(eval.thresholds.sequential_read_churn, 6);
    }

    #[test]
    fn sequential_read_churn_streak_broken_by_round_gap() {
        // Rounds 0,1,2 (single-tool) then gap (round 3 missing) then
        // rounds 4,5,6,7,8,9,10,11,12 (single-tool). Without gap-awareness
        // the streak would be 12; with it, the two segments are 3 and 9.
        let mut records = Vec::new();
        for r in [0, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12] {
            records.push(record_in_round("read_file", r, None));
        }
        let eval = evaluate_tool_call_records("explore", &[], &records, 0, false, 0.3);
        let streak = eval.signals.iter().find_map(|s| match s {
            EvalSignal::SequentialReadChurn(n) => Some(*n),
            _ => None,
        });
        // Longest contiguous segment is 9 (rounds 4-12), which exceeds
        // the threshold of 8.
        assert_eq!(
            streak,
            Some(9),
            "gap must break streak; expected 9, got {:?}",
            eval.signals
        );
    }

    #[test]
    fn sequential_read_churn_gap_splits_below_threshold() {
        // Two segments of 4 separated by a gap — neither reaches threshold.
        let mut records = Vec::new();
        for r in [0, 1, 2, 3, 8, 9, 10, 11] {
            records.push(record_in_round("read_file", r, None));
        }
        let eval = evaluate_tool_call_records("explore", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::SequentialReadChurn(_))),
            "two segments of 4 with gap should not trigger; got {:?}",
            eval.signals
        );
    }

    // ─── Graduated penalty calibration ─────────────────────────────────
    //
    // Formula: penalty = (0.05 + 0.01 * (streak - THRESHOLD)).clamp(0.05, 0.20)
    // We verify three points: at threshold, mid-range, and cap.
    // To isolate the churn penalty from other quality factors, we compare
    // two evaluations with the SAME number of records — one with all
    // single-tool rounds (triggers churn), one with all records in a
    // single round (no churn). The quality delta is purely the penalty.

    fn churn_penalty_for_streak(streak: usize) -> f64 {
        // All-single-tool-rounds: triggers SequentialReadChurn.
        let churn_records: Vec<_> = (0..streak as u32)
            .map(|r| record_in_round("read_file", r, None))
            .collect();
        // Same records but all in round 0: one round with `streak` tools,
        // so longest single-tool streak = 0 → no churn penalty.
        let batched_records: Vec<_> = (0..streak)
            .map(|_| record_in_round("read_file", 0, None))
            .collect();
        let eval_churn = evaluate_tool_call_records("q", &[], &churn_records, 0, false, 0.3);
        let eval_batched = evaluate_tool_call_records("q", &[], &batched_records, 0, false, 0.3);
        eval_batched.quality - eval_churn.quality
    }

    #[test]
    fn graduated_penalty_at_threshold_is_minimum() {
        let penalty = churn_penalty_for_streak(SEQUENTIAL_READ_CHURN_THRESHOLD); // 8
        assert!(
            (penalty - 0.05).abs() < 1e-9,
            "streak=8 should penalize 0.05, got {penalty}"
        );
    }

    #[test]
    fn graduated_penalty_scales_with_streak() {
        let penalty = churn_penalty_for_streak(18);
        assert!(
            (penalty - 0.15).abs() < 1e-9,
            "streak=18 should penalize 0.15, got {penalty}"
        );
    }

    #[test]
    fn graduated_penalty_caps_at_maximum() {
        let penalty = churn_penalty_for_streak(30);
        assert!(
            (penalty - 0.20).abs() < 1e-9,
            "streak=30 should cap at 0.20, got {penalty}"
        );
    }

    // ─── Redundant overlapping reads detection ──────────────────────────
    //
    // Calibration fixtures (real session shapes; see commit message):
    //   - 8ba9d165 turn 2: 4×`sed -n '159,200p' execution_phase.rs`,
    //     4×`sed -n '840,870p' …`, etc. — count >= 17.
    //   - eafda07e turn 2: 19 redundant reads, 18 rounds, 259k tokens.
    // Threshold 3 catches all known waste fixtures while leaving healthy
    // turns silent.

    fn record_with_args(name: &str, round: u32, args_full: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: Some(args_full.chars().take(80).collect()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            batch_id: None,
            parallel: Some(false),
            round: Some(round),
            args_full: Some(args_full.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn redundant_reads_detects_repeated_sed_ranges_in_same_file() {
        // Mirrors 8ba9d165 turn 2: same line range read 4 times across rounds,
        // no edits in between — pure waste.
        let records = vec![
            record_with_args(
                "bash",
                0,
                "sed -n '159,200p' rust/crates/runtime/src/turn/agentic_loop_execution_phase.rs",
            ),
            record_with_args(
                "bash",
                3,
                "sed -n '159,200p' rust/crates/runtime/src/turn/agentic_loop_execution_phase.rs",
            ),
            record_with_args(
                "bash",
                6,
                "sed -n '159,200p' rust/crates/runtime/src/turn/agentic_loop_execution_phase.rs",
            ),
            record_with_args(
                "bash",
                9,
                "sed -n '159,200p' rust/crates/runtime/src/turn/agentic_loop_execution_phase.rs",
            ),
        ];
        let eval = evaluate_tool_call_records("修复优化", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantOverlappingReads(n) => Some(*n),
            _ => None,
        });
        assert_eq!(
            count,
            Some(3), // 3 reads after the first overlap = 3 redundant events
            "expected RedundantOverlappingReads(3), got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_reads_detects_overlapping_but_not_identical_ranges() {
        // Reading 100-150 then 120-180 = same content overlap; should count.
        let records = vec![
            record_with_args("bash", 0, "sed -n '100,150p' src/foo.rs"),
            record_with_args("bash", 1, "sed -n '120,180p' src/foo.rs"),
            record_with_args("bash", 2, "sed -n '140,200p' src/foo.rs"),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantOverlappingReads(n) => Some(*n),
            _ => None,
        });
        // calls 2 and 3 each overlap a prior, and threshold=3 fires only at 3.
        assert_eq!(
            count, None,
            "only 2 redundant — below threshold 3, must be silent"
        );
    }

    #[test]
    fn redundant_reads_excludes_mutate_then_verify_pattern() {
        // Read → edit → read of same range is healthy verification, not waste.
        // We need 4 reads to potentially cross threshold; with an edit
        // between the first 2 and last 2, the count must reset and stay <3.
        let records = vec![
            record_with_args("bash", 0, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 1, "sed -n '10,50p' src/foo.rs"),
            // Edit invalidates per-file history.
            record_with_args(
                "edit",
                2,
                r#"{"path":"src/foo.rs","old_str":"x","new_str":"y"}"#,
            ),
            record_with_args("bash", 3, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 4, "sed -n '10,50p' src/foo.rs"),
        ];
        let eval = evaluate_tool_call_records("fix bug", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantOverlappingReads(n) => Some(*n),
            _ => None,
        });
        // Pre-edit: 1 redundant. Post-edit: 1 redundant. Total = 2 < threshold(3).
        assert_eq!(
            count, None,
            "mutate→verify pattern must not flag — got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_reads_does_not_flag_distinct_files() {
        // Three reads of different files = no overlap, no signal.
        let records = vec![
            record_with_args("bash", 0, "sed -n '1,50p' src/a.rs"),
            record_with_args("bash", 1, "sed -n '1,50p' src/b.rs"),
            record_with_args("bash", 2, "sed -n '1,50p' src/c.rs"),
            record_with_args("bash", 3, "sed -n '1,50p' src/d.rs"),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RedundantOverlappingReads(_))),
            "distinct files should not trigger redundancy; got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_reads_recognizes_view_tool_with_overlapping_ranges() {
        // The native `view` tool with overlapping `view_range` should also
        // count — the failure mode is identical regardless of bash vs view.
        let records = vec![
            record_with_args("view", 0, r#"{"path":"src/foo.rs","view_range":[10,50]}"#),
            record_with_args("view", 1, r#"{"path":"src/foo.rs","view_range":[20,60]}"#),
            record_with_args("view", 2, r#"{"path":"src/foo.rs","view_range":[30,70]}"#),
            record_with_args("view", 3, r#"{"path":"src/foo.rs","view_range":[40,80]}"#),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        let count = eval.signals.iter().find_map(|s| match s {
            EvalSignal::RedundantOverlappingReads(n) => Some(*n),
            _ => None,
        });
        assert_eq!(count, Some(3), "got {:?}", eval.signals);
    }

    #[test]
    fn redundant_reads_signal_serializes_to_json() {
        let v = eval_signal_to_json(&EvalSignal::RedundantOverlappingReads(5));
        assert_eq!(v["kind"], "redundant_overlapping_reads");
        assert_eq!(v["count"], 5);
        assert_eq!(v["threshold"], REDUNDANT_OVERLAPPING_READS_THRESHOLD as i64);
        assert!(v["message"].as_str().unwrap().contains("redundant"));
    }

    #[test]
    fn redundant_reads_custom_threshold_flows_into_journal_json() {
        let records = vec![
            record_with_args("bash", 0, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 1, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 2, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 3, "sed -n '10,50p' src/foo.rs"),
        ];
        let eval = evaluate_tool_call_records_with_thresholds(
            "fix bug",
            &[],
            &records,
            0,
            false,
            0.3,
            EvaluationThresholds {
                redundant_overlapping_reads: 3,
                ..Default::default()
            },
        );
        let event = build_turn_evaluation_journal_event(
            Some("sess-1"),
            Some(1),
            "cli_repl",
            "fix bug",
            &[],
            &records,
            0,
            false,
            0.3,
            &eval,
        );
        let metadata = event.metadata.expect("turn evaluation metadata");
        let redundant = metadata["signals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|signal| signal["kind"] == "redundant_overlapping_reads")
            .expect("redundant signal");
        assert_eq!(redundant["count"], 3);
        assert_eq!(redundant["threshold"], 3);
    }

    #[test]
    fn redundant_reads_below_threshold_is_silent() {
        // 2 redundant reads (threshold=3) must not emit signal.
        let records = vec![
            record_with_args("bash", 0, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 1, "sed -n '10,50p' src/foo.rs"),
            record_with_args("bash", 2, "sed -n '10,50p' src/foo.rs"),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RedundantOverlappingReads(_))),
            "2 redundant reads must be silent (threshold=3); got {:?}",
            eval.signals
        );
    }

    #[test]
    fn redundant_reads_grep_and_other_search_tools_not_counted() {
        // grep/ls/glob are search tools, not "read region of file" — they
        // must not contribute to redundancy even if same path repeats.
        let records = vec![
            record_with_args("bash", 0, "grep -n 'foo' src/x.rs"),
            record_with_args("bash", 1, "grep -n 'bar' src/x.rs"),
            record_with_args("bash", 2, "grep -n 'baz' src/x.rs"),
            record_with_args("bash", 3, "grep -n 'qux' src/x.rs"),
        ];
        let eval = evaluate_tool_call_records("q", &[], &records, 0, false, 0.3);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RedundantOverlappingReads(_))),
            "grep calls must not be classified as reads; got {:?}",
            eval.signals
        );
    }
}

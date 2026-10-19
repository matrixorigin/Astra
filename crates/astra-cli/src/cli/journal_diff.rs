//! `astra journal diff <A> <B>` — compare two session journals.
//!
//! Compares two sessions on the axes that matter for regression
//! debugging: tool-call sequence, per-event-type counts, aggregate
//! tokens, and final-text presence/similarity.  The intended use is:
//!
//! 1. Developer runs the harness on a case before a change and after,
//!    captures both session ids.
//! 2. `astra journal diff <before> <after>` — one command that tells
//!    them whether the runs diverged.
//!
//! Rendered as ASCII in text mode, structured JSON in `--format json`.
//! Both formats surface the same set of deltas; JSON is for CI /
//! dashboard consumers.
//!
//! Scope cut: we do not diff LLM message bodies — those are usually
//! stochastic across runs. Structural events + counts + final-text
//! presence cover the "did behavior change" question without
//! drowning the reviewer in wall-of-text diffs.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use astra_services::session_journal::{self, JournalEvent, JournalEventType, ToolCallRecord};

use crate::cli::cli_config::cli_args;
use crate::cli::journal_digest::{self, producer_run_id as run_id_of};

#[derive(Debug, Clone, Serialize)]
pub struct JournalDiff {
    pub a_session: String,
    pub b_session: String,
    /// Per-event-type counts for A (sorted by type name for stable diff).
    pub a_event_counts: BTreeMap<String, u32>,
    /// Per-event-type counts for B.
    pub b_event_counts: BTreeMap<String, u32>,
    /// Event types present in only one side. Surfaces "new kind of
    /// event appeared" regressions.
    pub only_in_a: Vec<String>,
    pub only_in_b: Vec<String>,
    /// Per-event-type count differences (b - a). Only entries where
    /// the delta is non-zero are included.
    pub count_deltas: BTreeMap<String, i64>,
    /// Ordered sequence of tool_name values called. Used for sequence-
    /// level diff: "A called [Read, Grep, Read]; B called [Read, Read]".
    pub a_tool_sequence: Vec<String>,
    pub b_tool_sequence: Vec<String>,
    /// Executed tool calls per run. Taken from each turn's authoritative
    /// `tool_count` aggregate, so it stays correct when per-call detail
    /// (`tool_calls`) was not recorded (e.g. server-owned CLI turns).
    pub a_tool_calls_executed: u64,
    pub b_tool_calls_executed: u64,
    /// False when the run executed more calls than its per-call detail
    /// covers; the sequence is then partial and must not be read as
    /// the full call list.
    pub a_tool_sequence_complete: bool,
    pub b_tool_sequence_complete: bool,
    /// Known root terminal tokens; excludes round observations and child work.
    pub a_tokens: TokenTotals,
    pub b_tokens: TokenTotals,
    /// b - a, signed. Negative = B used fewer tokens (usually good).
    pub token_deltas: TokenDeltas,
    /// Whether each run produced a non-empty final assistant output.
    pub a_has_final_text: bool,
    pub b_has_final_text: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenTotals {
    pub prompt: u64,
    pub completion: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenDeltas {
    pub prompt: i64,
    pub completion: i64,
    pub total: i64,
}

/// Load two journals + compute the diff. Returns `Err` only for fatal
/// io / parse failures on either session; structural differences are
/// always returned as Ok.
pub fn compute_diff(a_session: &str, b_session: &str) -> Result<JournalDiff, String> {
    let a_events =
        session_journal::read_journal(a_session).map_err(|e| format!("{a_session}: {e}"))?;
    let b_events =
        session_journal::read_journal(b_session).map_err(|e| format!("{b_session}: {e}"))?;
    Ok(diff_events(a_session, b_session, &a_events, &b_events))
}

/// Pure fold: no filesystem IO. Exposed as pub(crate) for tests.
pub(crate) fn diff_events(
    a_session: &str,
    b_session: &str,
    a_events: &[JournalEvent],
    b_events: &[JournalEvent],
) -> JournalDiff {
    let a_counts = event_counts(a_events);
    let b_counts = event_counts(b_events);
    let only_in_a: Vec<String> = a_counts
        .keys()
        .filter(|k| !b_counts.contains_key(*k))
        .cloned()
        .collect();
    let only_in_b: Vec<String> = b_counts
        .keys()
        .filter(|k| !a_counts.contains_key(*k))
        .cloned()
        .collect();
    let mut count_deltas = BTreeMap::new();
    let mut all_keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    all_keys.extend(a_counts.keys().map(String::as_str));
    all_keys.extend(b_counts.keys().map(String::as_str));
    for k in all_keys {
        let a = a_counts.get(k).copied().unwrap_or(0) as i64;
        let b = b_counts.get(k).copied().unwrap_or(0) as i64;
        let d = b - a;
        if d != 0 {
            count_deltas.insert(k.to_string(), d);
        }
    }

    let a_tokens = token_totals(a_events);
    let b_tokens = token_totals(b_events);
    let token_deltas = TokenDeltas {
        prompt: saturating_delta(a_tokens.prompt, b_tokens.prompt),
        completion: saturating_delta(a_tokens.completion, b_tokens.completion),
        total: saturating_delta(a_tokens.total, b_tokens.total),
    };

    let a_tools = tool_activity(a_events);
    let b_tools = tool_activity(b_events);

    JournalDiff {
        a_session: a_session.to_string(),
        b_session: b_session.to_string(),
        a_event_counts: a_counts,
        b_event_counts: b_counts,
        only_in_a,
        only_in_b,
        count_deltas,
        a_tool_sequence: a_tools.sequence,
        b_tool_sequence: b_tools.sequence,
        a_tool_calls_executed: a_tools.executed,
        b_tool_calls_executed: b_tools.executed,
        a_tool_sequence_complete: a_tools.complete,
        b_tool_sequence_complete: b_tools.complete,
        a_tokens,
        b_tokens,
        token_deltas,
        a_has_final_text: has_final_text(a_events),
        b_has_final_text: has_final_text(b_events),
    }
}

fn saturating_delta(a: u64, b: u64) -> i64 {
    let a_i: i128 = a as i128;
    let b_i: i128 = b as i128;
    let d = b_i - a_i;
    d.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn event_counts(events: &[JournalEvent]) -> BTreeMap<String, u32> {
    let mut out: BTreeMap<String, u32> = BTreeMap::new();
    for e in events {
        let k = event_type_str(&e.event_type);
        let count = out.entry(k).or_insert(0);
        *count = count.saturating_add(1);
    }
    out
}

fn token_totals(events: &[JournalEvent]) -> TokenTotals {
    let usage = journal_digest::root_terminal_usage(events);
    TokenTotals {
        prompt: usage.prompt,
        completion: usage.completion,
        total: usage.prompt.saturating_add(usage.completion),
    }
}

struct ToolActivity {
    sequence: Vec<String>,
    executed: u64,
    complete: bool,
}

fn is_terminal(e: &JournalEvent) -> bool {
    matches!(
        e.event_type,
        JournalEventType::Turn | JournalEventType::TurnError
    )
}

/// A child observation cannot join an anonymous root terminal. Typed producer
/// classification is shared with digest; metadata-only rounds also carry purpose.
fn is_child_round(e: &JournalEvent) -> bool {
    journal_digest::is_child_producer(e)
        || (e.producer_scope.is_none()
            && e.metadata
                .as_ref()
                .and_then(|m| m.get("purpose"))
                .and_then(|p| p.as_str())
                == Some("sub_agent"))
}

/// Terminal (`turn` / `turn_error`) positions indexed once per snapshot, so
/// resolving a round's owner is a few binary searches instead of a scan.
#[derive(Default)]
struct TerminalIndex<'a> {
    /// (turn, run id) -> terminals carrying that run id.
    by_run: HashMap<(u32, &'a str), Vec<usize>>,
    /// turn -> terminals without a run id.
    anonymous: HashMap<u32, Vec<usize>>,
    /// turn -> all terminals.
    any: HashMap<u32, Vec<usize>>,
}

impl<'a> TerminalIndex<'a> {
    fn new(events: &'a [JournalEvent]) -> Self {
        let mut idx = Self::default();
        for (i, e) in events.iter().enumerate() {
            let Some(turn) = e.turn.filter(|_| is_terminal(e)) else {
                continue;
            };
            match run_id_of(e) {
                Some(run) => idx.by_run.entry((turn, run)).or_default().push(i),
                None => idx.anonymous.entry(turn).or_default().push(i),
            }
            idx.any.entry(turn).or_default().push(i);
        }
        idx
    }

    /// Index of the terminal that owns the round at `round_idx`, searched
    /// over the whole snapshot: a round can be written after its terminal.
    /// A turn number alone does not identify an attempt, so when both sides
    /// carry a run id they must match. A root round without a run id (or a
    /// terminal without one) falls back to the turn number. A child round
    /// only joins a terminal of its own run. Among candidates, an exact
    /// run-id match wins, then the first terminal after the round, then the
    /// nearest one before it.
    fn owner(
        &self,
        events: &[JournalEvent],
        round_idx: usize,
        inferred_scope: Option<(&str, bool)>,
    ) -> Option<usize> {
        let round = &events[round_idx];
        let turn = round.turn?;
        let nearest = |list: Option<&Vec<usize>>| {
            let list = list?;
            let after = list.partition_point(|&i| i <= round_idx);
            list.get(after)
                .or_else(|| after.checked_sub(1).and_then(|b| list.get(b)))
                .copied()
        };
        let child = is_child_round(round) || inferred_scope.is_some_and(|(_, child)| child);
        match run_id_of(round).or(inferred_scope.map(|(run, _)| run)) {
            Some(run) => nearest(self.by_run.get(&(turn, run))).or_else(|| {
                (!child)
                    .then(|| nearest(self.anonymous.get(&turn)))
                    .flatten()
            }),
            None if child => None,
            None => nearest(self.any.get(&turn)),
        }
    }
}

/// Non-empty per-call observations from rounds and standalone failures.
fn round_calls(e: &JournalEvent) -> Option<&Vec<ToolCallRecord>> {
    matches!(
        e.event_type,
        JournalEventType::LlmRound | JournalEventType::ToolCallError
    )
    .then_some(e.tool_calls.as_ref())
    .flatten()
    .filter(|c| !c.is_empty())
}

type CallRunIndex<'a> = HashMap<(Option<u32>, &'a str), Option<(&'a str, bool)>>;

type RoundCallIndex<'a> = HashMap<(Option<u32>, &'a str, &'a str), (usize, usize)>;

fn inferred_error_scope<'a>(
    event: &JournalEvent,
    call_runs: &CallRunIndex<'a>,
) -> Option<(&'a str, bool)> {
    if event.event_type != JournalEventType::ToolCallError || run_id_of(event).is_some() {
        return None;
    }
    let [call] = event.tool_calls.as_deref()? else {
        return None;
    };
    call_runs
        .get(&(event.turn, call.tool_call_id.as_deref()?))
        .copied()
        .flatten()
}

/// A matching round owns call order; an immediately persisted error is another
/// observation of that call, even when terminal detail only covers other calls.
fn normalized_error<'a>(
    events: &'a [JournalEvent],
    terminals: &TerminalIndex<'_>,
    call_runs: &CallRunIndex<'a>,
    round_positions: &RoundCallIndex<'a>,
    event_idx: usize,
) -> Option<((usize, usize), &'a ToolCallRecord)> {
    let event = &events[event_idx];
    if event.event_type != JournalEventType::ToolCallError {
        return None;
    }
    let [call] = event.tool_calls.as_deref()? else {
        return None;
    };
    let inferred = inferred_error_scope(event, call_runs);
    let run = run_id_of(event).or(inferred.map(|(run, _)| run))?;
    let &(round, offset) =
        round_positions.get(&(event.turn, run, call.tool_call_id.as_deref()?))?;
    let representative = &events[round].tool_calls.as_ref()?[offset];
    if call.name != representative.name
        || call.was_executed() != representative.was_executed()
        || terminals.owner(events, event_idx, inferred) != terminals.owner(events, round, None)
    {
        return None;
    }
    Some(((round, offset), representative))
}

fn tool_activity(events: &[JournalEvent]) -> ToolActivity {
    // Terminals own authoritative counts and prefer their own detail over round
    // projections. Resolve observations across the complete snapshot, including
    // late writes. Anonymous errors require a unique call/run witness: turn numbers
    // can be reused after an interrupted attempt. Retain unowned observations.
    // Place terminal records by call ID or round, preserving interleaved activity.
    let mut out = ToolActivity {
        sequence: Vec::new(),
        executed: 0,
        complete: true,
    };
    let index = TerminalIndex::new(events);
    // terminal index -> indices of the rounds it owns (journal order).
    let mut owned: HashMap<usize, Vec<usize>> = HashMap::new();
    // Sequence chunks keyed by the journal position they are emitted at.
    let mut slots: BTreeMap<(usize, usize, usize), &ToolCallRecord> = BTreeMap::new();
    let mut orphan_ids = std::collections::HashSet::new();
    // Production ToolCallError records lack a run ID. Resolve only an
    // unambiguous (turn, call ID) witness; never merge different known runs.
    let mut call_runs: CallRunIndex<'_> = HashMap::new();
    let mut round_positions: RoundCallIndex<'_> = HashMap::new();
    // These lookups serve error admission and normalization only.
    if events
        .iter()
        .any(|event| event.event_type == JournalEventType::ToolCallError)
    {
        for (idx, e) in events.iter().enumerate() {
            if let Some(run) = run_id_of(e) {
                for (offset, call) in e.tool_calls.iter().flatten().enumerate() {
                    if let Some(id) = call.tool_call_id.as_deref() {
                        if e.event_type == JournalEventType::LlmRound {
                            round_positions
                                .entry((e.turn, run, id))
                                .or_insert((idx, offset));
                        }
                        call_runs
                            .entry((e.turn, id))
                            .and_modify(|known| {
                                if let Some((known_run, child)) = known {
                                    if *known_run == run {
                                        *child |= is_child_round(e);
                                    } else {
                                        *known = None;
                                    }
                                }
                            })
                            .or_insert(Some((run, is_child_round(e))));
                    }
                }
            }
        }
    }
    for (idx, e) in events.iter().enumerate() {
        if let Some(calls) = round_calls(e) {
            let anonymous_error =
                e.event_type == JournalEventType::ToolCallError && run_id_of(e).is_none();
            let inferred_scope = inferred_error_scope(e, &call_runs);
            let unresolved_error = anonymous_error && inferred_scope.is_none();
            let owner = (!unresolved_error)
                .then(|| index.owner(events, idx, inferred_scope))
                .flatten();
            match owner {
                Some(t) => owned.entry(t).or_default().push(idx),
                None => {
                    let normalized =
                        normalized_error(events, &index, &call_runs, &round_positions, idx);
                    for (offset, call) in calls.iter().enumerate() {
                        let ((position, within), call) =
                            normalized.unwrap_or(((idx, offset), call));
                        if call.tool_call_id.as_deref().is_some_and(|id| {
                            !orphan_ids.insert((
                                e.turn,
                                run_id_of(e).or(inferred_scope.map(|(run, _)| run)),
                                unresolved_error,
                                id,
                            ))
                        }) {
                            continue;
                        }
                        if unresolved_error && call.was_executed() {
                            out.complete = false;
                        }
                        out.executed = out.executed.saturating_add(u64::from(call.was_executed()));
                        slots.insert((position, within, 0), call);
                    }
                }
            }
        }
    }
    for (idx, e) in events.iter().enumerate() {
        if !is_terminal(e) {
            continue;
        }
        let rounds = owned.remove(&idx).unwrap_or_default();
        let mut locations = HashMap::new();
        let mut round_locations = HashMap::new();
        for &r in &rounds {
            let normalized = normalized_error(events, &index, &call_runs, &round_positions, r);
            for (offset, call) in round_calls(&events[r]).into_iter().flatten().enumerate() {
                let (position, call) = normalized.unwrap_or(((r, offset), call));
                if let Some(id) = call.tool_call_id.as_deref() {
                    locations.entry(id).or_insert(position);
                }
                if events[r].event_type == JournalEventType::LlmRound {
                    if let Some(round) = call.round {
                        round_locations.entry(round).or_insert(r);
                    }
                }
            }
        }
        let first_round = rounds
            .iter()
            .copied()
            .find(|&r| events[r].event_type == JournalEventType::LlmRound)
            .map_or(idx, |r| r.min(idx));
        let mut seen = std::collections::HashSet::new();
        let mut detailed = match e.tool_calls.as_ref().filter(|c| !c.is_empty()) {
            Some(calls) => {
                let mut count = 0;
                for (offset, call) in calls.iter().enumerate() {
                    if call
                        .tool_call_id
                        .as_deref()
                        .is_some_and(|id| !seen.insert(id))
                    {
                        continue;
                    }
                    let (position, within) = call
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| locations.get(id).copied())
                        .or_else(|| {
                            call.round
                                .and_then(|round| round_locations.get(&round).map(|&r| (r, offset)))
                        })
                        .unwrap_or((
                            if call.tool_call_id.is_some() {
                                idx
                            } else {
                                first_round
                            },
                            offset,
                        ));
                    slots.insert((position, within, offset + 1), call);
                    count += u64::from(call.was_executed());
                }
                count
            }
            None => 0,
        };
        let terminal_detail = e.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
        for &r in &rounds {
            // Terminal records supersede their round projections,
            // but a standalone failure remains evidence unless its ID is covered.
            if terminal_detail && events[r].event_type != JournalEventType::ToolCallError {
                continue;
            }
            let normalized = normalized_error(events, &index, &call_runs, &round_positions, r);
            for (offset, call) in round_calls(&events[r]).into_iter().flatten().enumerate() {
                let ((position, within), call) = normalized.unwrap_or(((r, offset), call));
                if call
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| !seen.insert(id))
                {
                    continue;
                }
                slots.insert((position, within, 0), call);
                detailed += u64::from(call.was_executed());
            }
        }
        let executed = e.tool_count.map(u64::from).unwrap_or(detailed);
        if detailed != executed {
            out.complete = false;
        }
        out.executed = out.executed.saturating_add(executed);
    }
    out.sequence = slots
        .into_values()
        .filter(|record| record.was_executed())
        .map(|record| record.name.clone())
        .collect();
    out
}

fn render_tool_sequence(label: &str, seq: &[String], executed: u64, complete: bool) -> String {
    if complete {
        format!("tool sequence {label} ({} calls): {seq:?}\n", seq.len())
    } else {
        format!(
            "tool sequence {label} ({executed} executed calls; detailed sequence \
             unavailable/incomplete, {} recorded): {seq:?}\n",
            seq.len()
        )
    }
}

fn has_final_text(events: &[JournalEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.event_type,
                JournalEventType::Turn | JournalEventType::LlmRound
            )
        })
        .and_then(|e| e.assistant_output.as_deref())
        .is_some_and(|s| !s.trim().is_empty())
}

/// Serde variant name as a stable string. Goes through the
/// `#[serde(rename_all = ...)]` attribute on `JournalEventType`,
/// producing the exact string that appears in the jsonl `type` field
/// — so a grepper can cross-reference `journal diff` output against
/// `jq 'select(.type=="X")'` without guessing.
fn event_type_str(t: &JournalEventType) -> String {
    // `serde_json::to_value` on a unit-variant enum yields a JSON
    // string — the same form used in the jsonl `type` field. Strip
    // the surrounding quotes to get the bare name. Defensive:
    // serialization errors and non-string shapes fall back to
    // "unknown" so the diff tool never panics on a future variant.
    match serde_json::to_value(t) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "unknown".to_string(),
    }
}

pub fn render_text(diff: &JournalDiff) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "=== journal diff ===\n  A: {}\n  B: {}\n",
        diff.a_session, diff.b_session
    ));
    if diff.a_session == diff.b_session {
        s.push_str("  ⚠ Warning: A and B resolve to the same session — diff will be zero.\n");
    }
    s.push('\n');

    s.push_str("token totals  (A → B,  Δ = B - A)\n");
    s.push_str(&format!(
        "  prompt:     {} → {}  (Δ {})\n",
        diff.a_tokens.prompt, diff.b_tokens.prompt, diff.token_deltas.prompt
    ));
    s.push_str(&format!(
        "  completion: {} → {}  (Δ {})\n",
        diff.a_tokens.completion, diff.b_tokens.completion, diff.token_deltas.completion
    ));
    s.push_str(&format!(
        "  total:      {} → {}  (Δ {})\n\n",
        diff.a_tokens.total, diff.b_tokens.total, diff.token_deltas.total
    ));

    s.push_str("final text: ");
    match (diff.a_has_final_text, diff.b_has_final_text) {
        (true, true) => s.push_str("both runs produced output\n\n"),
        (false, false) => s.push_str("NEITHER run produced output\n\n"),
        (true, false) => s.push_str("A produced output; B did NOT\n\n"),
        (false, true) => s.push_str("B produced output; A did NOT\n\n"),
    }

    s.push_str("event-type counts (Δ non-zero only):\n");
    if diff.count_deltas.is_empty() {
        s.push_str("  (no differences)\n");
    } else {
        for (k, d) in &diff.count_deltas {
            let a = diff.a_event_counts.get(k).copied().unwrap_or(0);
            let b = diff.b_event_counts.get(k).copied().unwrap_or(0);
            s.push_str(&format!("  {k:40} {a} → {b}  (Δ {d:+})\n"));
        }
    }
    if !diff.only_in_a.is_empty() {
        s.push_str(&format!("  only in A: {:?}\n", diff.only_in_a));
    }
    if !diff.only_in_b.is_empty() {
        s.push_str(&format!("  only in B: {:?}\n", diff.only_in_b));
    }

    s.push('\n');
    s.push_str(&render_tool_sequence(
        "A",
        &diff.a_tool_sequence,
        diff.a_tool_calls_executed,
        diff.a_tool_sequence_complete,
    ));
    s.push_str(&render_tool_sequence(
        "B",
        &diff.b_tool_sequence,
        diff.b_tool_calls_executed,
        diff.b_tool_sequence_complete,
    ));
    if diff.a_tool_sequence_complete && diff.b_tool_sequence_complete {
        if diff.a_tool_sequence != diff.b_tool_sequence {
            s.push_str("  → tool sequence DIFFERS\n");
        }
    } else if diff.a_tool_calls_executed != diff.b_tool_calls_executed {
        s.push_str(&format!(
            "  → executed tool-call count DIFFERS ({} → {}); sequences not comparable\n",
            diff.a_tool_calls_executed, diff.b_tool_calls_executed
        ));
    } else {
        s.push_str("  → sequences not comparable (detail incomplete)\n");
    }

    s
}

pub(crate) fn run_diff(args: &cli_args::JournalDiffArgs) -> Result<(), String> {
    let a = journal_digest::resolve_session_for_digest(Some(&args.a), None)?;
    let b = journal_digest::resolve_session_for_digest(Some(&args.b), None)?;
    let diff = compute_diff(&a, &b)?;
    let format = args.format.trim().to_ascii_lowercase();
    match format.as_str() {
        "" | "text" | "txt" => {
            stdout_print!("{}", render_text(&diff));
            Ok(())
        }
        "json" => {
            stdout_println!(
                "{}",
                serde_json::to_string_pretty(&diff).unwrap_or_default()
            );
            Ok(())
        }
        other => Err(format!(
            "invalid --format '{other}' (expected text or json)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{JournalEvent, diff_events, render_text};
    use serde_json::json;

    fn evt(raw: serde_json::Value) -> JournalEvent {
        serde_json::from_value(raw).expect("valid journal event")
    }

    #[test]
    fn identical_journals_have_zero_deltas() {
        let a = vec![evt(
            json!({"type":"turn","ts":"t1","session_id":"a","tokens_in":100,"tokens_out":10,"assistant_output":"hello"}),
        )];
        let b = a.clone();
        let d = diff_events("a", "b", &a, &b);
        assert_eq!(d.token_deltas.prompt, 0);
        assert_eq!(d.token_deltas.completion, 0);
        assert!(d.count_deltas.is_empty());
        assert!(d.only_in_a.is_empty());
        assert!(d.only_in_b.is_empty());
        assert!(d.a_has_final_text);
        assert!(d.b_has_final_text);
    }

    #[test]
    fn token_totals_use_terminals_once_instead_of_rounds_plus_terminals() {
        for terminal_input in [100, 150] {
            let round = evt(json!({"type":"llm_round", "ts":"t1", "session_id":"a",
                "turn":1, "round":0, "tokens_in":50, "tokens_out":5}));
            let events = vec![
                round.clone(),
                round,
                evt(json!({"type":"llm_round", "ts":"t2", "session_id":"a",
                    "turn":1, "round":1, "tokens_in":100, "tokens_out":10})),
                evt(json!({"type":"turn", "ts":"t3", "session_id":"a",
                    "turn":1, "tokens_in":terminal_input, "tokens_out":10})),
                evt(json!({"type":"turn_error", "ts":"t4", "session_id":"a",
                    "producer_scope":{"run_id":"root", "agent_id":"root"},
                    "tokens_in":20, "tokens_out":2})),
                evt(json!({"type":"turn_error", "ts":"t5", "session_id":"a",
                    "producer_scope":{"run_id":"child", "parent_run_id":"root", "agent_id":"child"},
                    "tokens_in":999, "tokens_out":999})),
                // Observations without a terminal are not qualified accounting.
                evt(json!({"type":"llm_round", "ts":"t6", "session_id":"a",
                    "turn":2, "tokens_in":500, "tokens_out":50})),
                evt(
                    json!({"type":"tool_call_error", "ts":"t7", "session_id":"a",
                    "tokens_in":700, "tokens_out":70}),
                ),
            ];
            let diff = diff_events("a", "b", &events, &[]);
            assert_eq!(diff.a_tokens.prompt, terminal_input + 20);
            assert_eq!(diff.a_tokens.completion, 12);
            let (tree, _, _) = crate::cli::journal_tree::fold_events_into_tree("a", &events);
            assert_eq!(tree.self_prompt_tokens, diff.a_tokens.prompt);
            assert_eq!(tree.self_completion_tokens, diff.a_tokens.completion);
        }
    }

    #[test]
    fn real_fixture_token_total_matches_digest_terminal_accounting() {
        let events: Vec<JournalEvent> =
            include_str!("../../../services/fixtures/real_session_0ac769_min.jsonl")
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        let diff = diff_events("a", "b", &events, &events);
        assert_eq!(diff.a_tokens.prompt, 33659);
        assert_eq!(diff.a_tokens.completion, 2855);
        let (tree, _, _) = crate::cli::journal_tree::fold_events_into_tree("a", &events);
        assert_eq!(tree.self_prompt_tokens, diff.a_tokens.prompt);
        assert_eq!(tree.self_completion_tokens, diff.a_tokens.completion);
    }

    #[test]
    fn token_deltas_are_signed_b_minus_a() {
        let a = vec![evt(
            json!({"type":"turn","ts":"t","session_id":"a","tokens_in":1000,"tokens_out":50}),
        )];
        let b = vec![evt(
            json!({"type":"turn","ts":"t","session_id":"b","tokens_in":800,"tokens_out":80}),
        )];
        let d = diff_events("a", "b", &a, &b);
        assert_eq!(d.token_deltas.prompt, -200); // B used fewer prompt tokens
        assert_eq!(d.token_deltas.completion, 30);
        assert_eq!(d.token_deltas.total, -170);
    }

    #[test]
    fn only_in_a_and_only_in_b_are_computed_correctly() {
        let a = vec![
            evt(json!({"type":"turn","ts":"t","session_id":"a"})),
            evt(json!({"type":"stall_detected","ts":"t","session_id":"a"})),
        ];
        let b = vec![
            evt(json!({"type":"turn","ts":"t","session_id":"b"})),
            evt(json!({"type":"compact","ts":"t","session_id":"b"})),
        ];
        let d = diff_events("a", "b", &a, &b);
        assert_eq!(d.only_in_a, vec!["stall_detected".to_string()]);
        assert_eq!(d.only_in_b, vec!["compact".to_string()]);
        // Shared turn count is zero-delta and must NOT appear in count_deltas.
        assert!(!d.count_deltas.contains_key("turn"));
    }

    #[test]
    fn tool_sequence_preserves_order() {
        // `tool_calls` is a Vec<ToolCallRecord> nested on Turn/LlmRound
        // events. The diff walks events in order + records in order.
        let a = vec![evt(json!({
            "type":"turn","ts":"t","session_id":"a",
            "tool_calls": [
                {"name":"Read","ok":true,"ms":0},
                {"name":"Grep","ok":true,"ms":0},
            ]
        }))];
        let b = vec![evt(json!({
            "type":"turn","ts":"t","session_id":"b",
            "tool_calls": [
                {"name":"Grep","ok":true,"ms":0},
                {"name":"Read","ok":true,"ms":0},
            ]
        }))];
        let d = diff_events("a", "b", &a, &b);
        assert_eq!(d.a_tool_sequence, vec!["Read", "Grep"]);
        assert_eq!(d.b_tool_sequence, vec!["Grep", "Read"]);
    }

    #[test]
    fn aggregate_only_turns_do_not_render_as_zero_calls() {
        // Server-owned CLI turns record `tool_count` / `tools_used` /
        // `tool_outcomes` but no per-call `tool_calls` detail (#784).
        // Missing detail must not be reported as "0 calls".
        let a = vec![
            evt(json!({
                "type":"turn","ts":"t1","session_id":"a","turn":1,"tool_count":6,
                "tools_used":["bash","read"],
                "tool_outcomes":{"requested":6,"executed":6,"succeeded":5,"failed":1,
                    "rejected":0,"reused":0,"suppressed":0,"deferred":0}
            })),
            evt(json!({"type":"turn","ts":"t2","session_id":"a","turn":2,"tool_count":0})),
        ];
        let b = vec![evt(json!({
            "type":"turn","ts":"t1","session_id":"b","turn":1,"tool_count":82
        }))];
        let d = diff_events("a", "b", &a, &b);
        let txt = render_text(&d);
        assert!(
            !txt.contains("(0 calls)"),
            "missing detail rendered as zero:\n{txt}"
        );
        assert!(txt.contains("6 executed"), "{txt}");
        assert!(txt.contains("82 executed"), "{txt}");
        assert!(txt.contains("unavailable"), "{txt}");
    }

    #[test]
    fn round_and_turn_records_for_same_turn_are_not_double_counted() {
        // Local runs embed per-call records on each llm_round AND on the
        // completed turn; the sequence must list each call once.
        let calls = json!([
            {"name":"Read","ok":true,"ms":0},
            {"name":"Grep","ok":true,"ms":0},
        ]);
        let a = vec![
            evt(
                json!({"type":"llm_round","ts":"t1","session_id":"a","turn":1,
                "tool_calls":[{"name":"Read","ok":true,"ms":0}]}),
            ),
            evt(
                json!({"type":"llm_round","ts":"t2","session_id":"a","turn":1,
                "tool_calls":[{"name":"Grep","ok":true,"ms":0}]}),
            ),
            evt(json!({"type":"turn","ts":"t3","session_id":"a","turn":1,
                "tool_count":2,"tool_calls":calls})),
        ];
        let d = diff_events("a", "a2", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["Read", "Grep"]);
    }

    fn rd(turn: u32, run: &str, name: &str) -> JournalEvent {
        evt(
            json!({"type":"llm_round","ts":"t","session_id":"a","turn":turn,
            "metadata":{"run_id":run},"tool_calls":[{"name":name,"ok":true,"ms":0}]}),
        )
    }

    #[test]
    fn failed_turns_are_included_in_the_activity_fold() {
        let a = vec![evt(
            json!({"type":"turn_error","ts":"t","session_id":"a","turn":1,
            "tool_calls":[{"name":"read_file","ok":true,"ms":0}]}),
        )];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file"]);
        let a = vec![evt(
            json!({"type":"turn_error","ts":"t","session_id":"a","turn":1,"tool_count":6}),
        )];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_calls_executed, 6);
        assert!(!d.a_tool_sequence_complete);
    }

    #[test]
    fn rounds_of_another_run_with_the_same_turn_are_kept() {
        let a = vec![
            rd(1, "run-a", "read_file"),
            evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":1,
                "metadata":{"run_id":"run-b"},"tool_calls":[{"name":"bash","ok":true,"ms":0}]}),
            ),
        ];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file", "bash"]);
        assert_eq!(d.a_tool_calls_executed, 2);
    }

    #[test]
    fn rounds_written_after_their_terminal_are_not_counted_twice() {
        let a = vec![
            evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":1,
                "metadata":{"run_id":"r1"}}),
            ),
            rd(1, "r1", "read_file"),
        ];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file"]);
        assert_eq!(d.a_tool_calls_executed, 1);
        assert!(d.a_tool_sequence_complete);
        // terminal also carries the detail: listed once
        let a = vec![
            evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":1,
                "metadata":{"run_id":"r1"},"tool_calls":[{"name":"read_file","ok":true,"ms":0}]}),
            ),
            rd(1, "r1", "read_file"),
        ];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file"]);
        assert_eq!(d.a_tool_calls_executed, 1);
    }

    #[test]
    fn unmatched_rounds_keep_their_chronological_position() {
        let a = vec![
            rd(1, "r1", "read_file"),
            evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":2,"tool_count":1,
                "metadata":{"run_id":"r2"},"tool_calls":[{"name":"bash","ok":true,"ms":0}]}),
            ),
        ];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file", "bash"]);
        assert_eq!(d.a_tool_calls_executed, 2);
    }

    #[test]
    fn matched_rounds_keep_their_position_among_unmatched_rounds() {
        // Root round (turn 1), then a child round with its own producer
        // scope and no turn, then the root terminal with only `tool_count`.
        let a = vec![
            rd(1, "root", "read_file"),
            evt(json!({"type":"llm_round","ts":"t","session_id":"a",
                "producer_scope":{"run_id":"child","parent_run_id":"root","agent_id":"worker"},
                "tool_calls":[{"name":"grep","ok":true,"ms":0}]})),
            evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":1,
                "metadata":{"run_id":"root"}}),
            ),
        ];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file", "grep"]);
        assert_eq!(d.a_tool_calls_executed, 2);
        assert!(d.a_tool_sequence_complete);
        // Same shape when the terminal carries its own per-call detail.
        let mut a = a;
        a[2] = evt(
            json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":1,
            "metadata":{"run_id":"root"},"tool_calls":[{"name":"read_file","ok":true,"ms":0}]}),
        );
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file", "grep"]);
        assert_eq!(d.a_tool_calls_executed, 2);
    }

    #[test]
    fn identified_child_round_is_not_folded_into_an_anonymous_terminal() {
        let child = |purpose: bool, scope: bool| {
            let mut v = json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
                "tool_calls":[{"name":"grep","ok":true,"ms":0}]});
            if scope {
                v["producer_scope"] =
                    json!({"run_id":"child","parent_run_id":"root","agent_id":"worker"});
            }
            if purpose {
                v["metadata"] = json!({"purpose":"sub_agent"});
            }
            evt(v)
        };
        let terminal = evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
            "tool_count":1,"tool_calls":[{"name":"read_file","ok":true,"ms":0}]}));
        for (purpose, scope) in [(true, true), (false, true), (true, false)] {
            let a = vec![child(purpose, scope), terminal.clone()];
            let d = diff_events("a", "b", &a, &[]);
            assert_eq!(
                d.a_tool_sequence,
                vec!["grep", "read_file"],
                "purpose={purpose} scope={scope}"
            );
            assert_eq!(d.a_tool_calls_executed, 2);
            assert!(d.a_tool_sequence_complete);
        }
        // Explicit root agent identity must not be mistaken for a child.
        let root = evt(
            json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
            "producer_scope":{"run_id":"root","agent_id":"root"},
            "tool_calls":[{"name":"read_file","ok":true,"ms":0}]}),
        );
        let diff = diff_events("a", "b", &[root, terminal.clone()], &[]);
        assert_eq!(diff.a_tool_calls_executed, 1);
        assert_eq!(diff.a_tool_sequence, ["read_file"]);
        assert!(diff.a_tool_sequence_complete);
        // A root round without child identity still uses the legacy fallback.
        let a = vec![rd(1, "root", "read_file"), terminal];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file"]);
        assert_eq!(d.a_tool_calls_executed, 1);
    }

    #[test]
    fn terminal_lookup_prefers_exact_run_then_nearest_following() {
        let term = |turn: u32, run: Option<&str>, name: &str| {
            let mut v = json!({"type":"turn","ts":"t","session_id":"a","turn":turn,
                "tool_count":1,"tool_calls":[{"name":name,"ok":true,"ms":0}]});
            if let Some(r) = run {
                v["metadata"] = json!({"run_id": r});
            }
            evt(v)
        };
        // Exact run match before the round beats an anonymous terminal after it.
        let a = vec![
            term(1, Some("r1"), "t_exact"),
            rd(1, "r1", "round"),
            term(1, None, "t_anon"),
        ];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["t_exact", "t_anon"]);
        assert_eq!(d.a_tool_calls_executed, 2);
        // Without a run id, the first following terminal of that turn wins.
        let a = vec![
            term(1, Some("x"), "t_before"),
            evt(
                json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
                "tool_calls":[{"name":"round","ok":true,"ms":0}]}),
            ),
            term(1, Some("y"), "t_after1"),
            term(1, Some("z"), "t_after2"),
        ];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["t_before", "t_after1", "t_after2"]);
        assert_eq!(d.a_tool_calls_executed, 3);
    }

    async fn assert_unwitnessed_error_after_restore(with_detail: bool) {
        use astra_services::session_journal::{JournalDirGuard, JournalWriter};
        use astra_services::session_restore::HybridRestoreService;

        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let empty = uuid::Uuid::new_v4().to_string();
        JournalWriter::new(&empty)
            .unwrap()
            .append(&JournalEvent::session_start(
                Some(&empty),
                Some("test-model"),
            ))
            .unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let writer = JournalWriter::new(&sid).unwrap();
        writer
            .append(&JournalEvent::session_start(Some(&sid), Some("test-model")))
            .unwrap();
        let failed = serde_json::from_value(json!({
            "tool_call_id":"aborted-call","name":"bash","ok":false,
            "ms":1,"error":"exit 1"
        }))
        .unwrap();
        let error = JournalEvent::tool_call_error(Some(&sid), 1, "bash", "exit 1", failed);
        assert!(super::run_id_of(&error).is_none());
        writer.append(&error).unwrap();
        let restored = HybridRestoreService::local_only()
            .restore_local_session(&sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.turn_count, 0);
        let mut terminal = JournalEvent::turn(
            Some(&sid),
            restored.turn_count + 1,
            Some("test-model"),
            "continue",
            "done",
            1,
            0,
            0,
            0,
        )
        .with_run_id(Some("later-run"));
        if with_detail {
            terminal.tool_calls = Some(vec![
                serde_json::from_value(json!({
                    "tool_call_id":"later-call","name":"read_file","ok":true,"ms":1
                }))
                .unwrap(),
            ]);
        }
        writer.append(&terminal).unwrap();
        let diff = super::compute_diff(&sid, &empty).unwrap();
        assert_eq!(diff.a_tool_calls_executed, 2);
        assert!(!diff.a_tool_sequence_complete);
        let expected = if with_detail {
            vec!["bash", "read_file"]
        } else {
            vec!["bash"]
        };
        assert_eq!(diff.a_tool_sequence, expected);
        let encoded = serde_json::to_value(&diff).unwrap();
        assert_eq!(encoded["a_tool_calls_executed"], 2);
        assert_eq!(encoded["a_tool_sequence_complete"], false);
        assert_eq!(encoded["a_tool_sequence"], json!(expected));
        let text = render_text(&diff);
        assert!(
            text.contains("2 executed calls; detailed sequence"),
            "{text}"
        );
        assert!(!text.contains("tool sequence DIFFERS"), "{text}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unwitnessed_error_survives_restore_with_detailed_later_turn() {
        assert_unwitnessed_error_after_restore(true).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unwitnessed_error_survives_restore_with_aggregate_later_turn() {
        assert_unwitnessed_error_after_restore(false).await;
    }

    #[test]
    fn execution_dispositions_agree_across_counts_sequences_text_and_json() {
        use astra_services::session_journal::ToolCallDisposition::{
            Deferred, Executed, Rejected, Reused, Suppressed,
        };
        let nonexecuted: Vec<_> = [Rejected, Reused, Suppressed, Deferred]
            .into_iter()
            .enumerate()
            .map(|(i, disposition)| {
                json!({"tool_call_id":format!("no-exec-{i}"),"name":"not-executed",
                    "ok":false,"ms":0,"disposition":disposition})
            })
            .collect();
        let mut mixed = vec![
            json!({"tool_call_id":"read","name":"read_file","ok":true,"ms":1,"disposition":Executed}),
            json!({"tool_call_id":"bash","name":"bash","ok":false,"ms":1,"disposition":Executed}),
        ];
        mixed.extend(nonexecuted.clone());
        for (records, count, names, complete) in [
            (mixed.clone(), 2, vec!["read_file", "bash"], true),
            (nonexecuted, 0, vec![], true),
            (mixed, 3, vec!["read_file", "bash"], false),
        ] {
            let events = [evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":1,
                "tool_count":count,"tool_calls":records}),
            )];
            let diff = diff_events("a", "b", &events, &[]);
            assert_eq!(diff.a_tool_calls_executed, count);
            assert_eq!(diff.a_tool_sequence, names);
            assert_eq!(diff.a_tool_sequence_complete, complete);
            let encoded = serde_json::to_value(&diff).unwrap();
            assert_eq!(encoded["a_tool_calls_executed"], count);
            assert_eq!(encoded["a_tool_sequence"], json!(names));
            assert_eq!(encoded["a_tool_sequence_complete"], complete);
            let text = render_text(&diff);
            let label = if complete {
                format!("({count} calls)")
            } else {
                format!("({count} executed calls;")
            };
            assert!(text.contains(&label), "{text}");
        }
    }

    #[test]
    fn nonexecuted_orphan_error_keeps_zero_execution_sequence_complete() {
        let record = serde_json::from_value(json!({
            "tool_call_id":"rejected-call","name":"bash","ok":false,"ms":0,
            "disposition":"rejected","error":"permission denied"
        }))
        .unwrap();
        let error =
            JournalEvent::tool_call_error(Some("a"), 1, "bash", "permission denied", record);
        let terminal = evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
            "tool_count":0,"metadata":{"run_id":"later-run"}}));
        for events in [vec![error.clone()], vec![error, terminal]] {
            let diff = diff_events("a", "b", &events, &[]);
            assert_eq!(diff.a_tool_calls_executed, 0);
            assert!(diff.a_tool_sequence.is_empty());
            assert!(diff.a_tool_sequence_complete);
            let encoded = serde_json::to_value(&diff).unwrap();
            assert_eq!(encoded["a_tool_sequence"], json!([]));
            assert_eq!(encoded["a_tool_sequence_complete"], true);
            assert!(render_text(&diff).contains("tool sequence A (0 calls): []"));
        }
    }

    #[test]
    fn round_order_is_invariant_under_redundant_error_observation() {
        use astra_services::session_journal::{
            JournalDirGuard, JournalWriter, LlmRoundRecord, TurnEventBuffer,
        };

        fn write_session(terminal_detail: Option<usize>, error: Option<(bool, bool)>) -> String {
            let sid = uuid::Uuid::new_v4().to_string();
            let read: super::ToolCallRecord = serde_json::from_value(json!({
                "tool_call_id":"read","name":"read_file","round":0,
                "start_offset_ms":0,"ok":true,"ms":1
            }))
            .unwrap();
            let failed: super::ToolCallRecord = serde_json::from_value(json!({
                "tool_call_id":"failed","name":"bash","round":0,
                "start_offset_ms":10,"ok":false,"ms":1,"error":"exit 1"
            }))
            .unwrap();
            let mut buffer = TurnEventBuffer::begin_turn(Some(&sid), 1);
            let mut record = LlmRoundRecord::new(astra_turn_types::InferencePurpose::PrimaryAgent);
            record.run_id = Some("root".into());
            record.tool_calls = Some(vec![read.clone(), failed.clone()]);
            record.tool_calls_returned = 2;
            buffer.record_llm_round(record);
            let mut events = buffer.drain();
            events[0].ts = "2026-10-19T00:00:01Z".into();
            if let Some((before_round, explicit_run)) = error {
                let mut event =
                    JournalEvent::tool_call_error(Some(&sid), 1, "bash", "exit 1", failed.clone());
                if explicit_run {
                    event = event.with_run_id(Some("root"));
                }
                event.ts = if before_round {
                    "2026-10-19T00:00:00Z"
                } else {
                    "2026-10-19T00:00:02Z"
                }
                .into();
                events.push(event);
            }
            if let Some(with_detail) = terminal_detail {
                let mut event = JournalEvent::turn(
                    Some(&sid),
                    1,
                    Some("test-model"),
                    "run",
                    "done",
                    2,
                    0,
                    0,
                    0,
                )
                .with_run_id(Some("root"));
                event.ts = "2026-10-19T00:00:03Z".into();
                if with_detail > 0 {
                    let mut calls = vec![read, failed];
                    calls.truncate(with_detail);
                    event = event.with_tool_calls(calls);
                }
                // Partial terminal data keeps its independent executed aggregate.
                event.tool_count = Some(2);
                events.push(event);
            }
            let writer = JournalWriter::new(&sid).unwrap();
            for event in events {
                writer.append(&event).unwrap();
            }
            sid
        }

        let dir = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(dir.path());
        for terminal in [None, Some(0), Some(1), Some(2)] {
            // Partial terminal detail needs the standalone error to supplement
            // the missing failure. Compare it to the complete-detail control.
            let control = write_session(
                if terminal == Some(1) {
                    Some(2)
                } else {
                    terminal
                },
                None,
            );
            for before_round in [true, false] {
                for explicit_run in [false, true] {
                    let sid = write_session(terminal, Some((before_round, explicit_run)));
                    let diff = super::compute_diff(&sid, &control).unwrap();
                    assert_eq!(
                        diff.a_tool_sequence,
                        ["read_file", "bash"],
                        "terminal={terminal:?}, before={before_round}, explicit={explicit_run}"
                    );
                    assert_eq!(diff.a_tool_sequence, diff.b_tool_sequence);
                    assert_eq!(diff.a_tool_calls_executed, 2);
                    assert!(diff.a_tool_sequence_complete);
                    let json = serde_json::to_value(&diff).unwrap();
                    assert_eq!(json["a_tool_sequence"], json!(["read_file", "bash"]));
                    assert_eq!(json["a_tool_calls_executed"], 2);
                    assert_eq!(json["a_tool_sequence_complete"], true);
                    assert!(!render_text(&diff).contains("tool sequence DIFFERS"));
                }
            }
        }
    }

    #[test]
    fn review_followup_production_error_without_run_id_is_not_counted_twice() {
        let record: super::ToolCallRecord = serde_json::from_value(json!({
            "tool_call_id":"failed-bash","name":"bash","round":0,
            "ok":false,"ms":1,"error":"exit 1"
        }))
        .unwrap();
        // Use the production constructor: ToolCallError has no run identity.
        let error = JournalEvent::tool_call_error(Some("a"), 1, "bash", "exit 1", record.clone());
        assert!(super::run_id_of(&error).is_none());
        let round = evt(
            json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
            "producer_scope":{"run_id":"root"},"tool_calls":[record]}),
        );
        let terminal = evt(
            json!({"type":"turn_error","ts":"t","session_id":"a","turn":1,
            "producer_scope":{"run_id":"root"},"tool_count":1}),
        );
        for events in [
            vec![error.clone(), round.clone()],
            vec![round.clone(), error.clone()],
            vec![error.clone(), round.clone(), terminal.clone()],
            vec![error.clone(), round.clone(), error.clone(), round.clone()],
            vec![
                error.clone(),
                round.clone(),
                error.clone(),
                round.clone(),
                terminal.clone(),
            ],
            vec![terminal, round.clone(), error],
        ] {
            let diff = diff_events("a", "b", &events, &[]);
            assert_eq!(diff.a_tool_sequence, ["bash"]);
            assert_eq!(diff.a_tool_calls_executed, 1);
            assert!(diff.a_tool_sequence_complete);
        }
        // Missing IDs and multi-call errors cannot inherit a round's identity.
        let failed = round.tool_calls.as_ref().unwrap()[0].clone();
        let mut missing_id = failed.clone();
        missing_id.tool_call_id = None;
        let missing = JournalEvent::tool_call_error(Some("a"), 1, "bash", "exit 1", missing_id);
        let mut multiple =
            JournalEvent::tool_call_error(Some("a"), 1, "bash", "exit 1", failed.clone());
        let mut another = failed;
        another.tool_call_id = Some("another-failure".into());
        multiple.tool_calls.as_mut().unwrap().push(another);
        for (error, retained) in [(missing, 2), (multiple, 3)] {
            let diff = diff_events("a", "b", &[round.clone(), error], &[]);
            assert_eq!(diff.a_tool_calls_executed, retained);
            assert!(!diff.a_tool_sequence_complete);
        }
        // Missing run identity on both projections is not a positive witness.
        // Keep uncertain error evidence separate from a runless round.
        let mut runless = round.clone();
        runless.producer_scope = None;
        let error = JournalEvent::tool_call_error(
            Some("a"),
            1,
            "bash",
            "exit 1",
            runless.tool_calls.as_ref().unwrap()[0].clone(),
        );
        for events in [
            vec![runless.clone(), error.clone(), error.clone()],
            vec![error, runless],
        ] {
            let diff = diff_events("a", "b", &events, &[]);
            assert_eq!(diff.a_tool_calls_executed, 2);
            assert!(!diff.a_tool_sequence_complete);
        }
        // A reused provider ID in a distinct known run is not a duplicate.
        let mut other = round.clone();
        other.producer_scope.as_mut().unwrap().run_id = "other".to_string();
        let diff = diff_events("a", "b", &[round, other], &[]);
        assert_eq!(diff.a_tool_sequence, ["bash", "bash"]);
        assert_eq!(diff.a_tool_calls_executed, 2);
    }

    #[test]
    fn review_followup_inferred_child_never_joins_anonymous_root() {
        let record: super::ToolCallRecord = serde_json::from_value(json!({
            "tool_call_id":"child-failure","name":"bash","ok":false,"ms":1
        }))
        .unwrap();
        let error = JournalEvent::tool_call_error(Some("a"), 1, "bash", "failed", record.clone());
        let child = evt(
            json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
            "producer_scope":{"run_id":"child","parent_run_id":"root"},"tool_calls":[record]}),
        );
        let root = evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":0}));
        for a in [
            vec![error.clone(), child.clone(), root.clone()],
            vec![root.clone(), child.clone(), error.clone()],
        ] {
            let d = diff_events("a", "b", &a, &[]);
            assert_eq!(d.a_tool_sequence, ["bash"]);
            assert_eq!(d.a_tool_calls_executed, 1);
            assert!(d.a_tool_sequence_complete);
        }
        let mut other = child.clone();
        other.producer_scope.as_mut().unwrap().run_id = "other".to_string();
        let d = diff_events("a", "b", &[error, child, other, root], &[]);
        assert_eq!(d.a_tool_calls_executed, 3);
        assert!(
            !d.a_tool_sequence_complete,
            "conflicting identities cannot prove complete attribution"
        );
    }

    #[test]
    fn review_followup_terminal_detail_preserves_interleaved_rounds() {
        let call = |id: &str, name: &str, round: u32| json!({"tool_call_id":id,"name":name,"ok":true,"ms":0,"round":round});
        let mut a = vec![
            evt(
                json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
                "metadata":{"run_id":"root"},"tool_calls":[call("a", "read_file", 0)]}),
            ),
            evt(json!({"type":"llm_round","ts":"t","session_id":"a",
                "producer_scope":{"run_id":"child","parent_run_id":"root","agent_id":"worker"},
                "tool_calls":[call("c", "grep", 0)]})),
            evt(
                json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
                "metadata":{"run_id":"root"},"tool_calls":[call("b", "bash", 1)]}),
            ),
            evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
                "metadata":{"run_id":"root"},"tool_count":2,
                "tool_calls":[call("a", "read_file", 0),call("b", "bash", 1)]})),
        ];
        let mut b = a.clone();
        b[3].tool_calls = None;
        let d = diff_events("a", "b", &a, &b);
        assert_eq!(d.a_tool_sequence, vec!["read_file", "grep", "bash"]);
        assert_eq!(d.a_tool_sequence, d.b_tool_sequence);
        assert_eq!(d.a_tool_calls_executed, 3);
        assert!(d.a_tool_sequence_complete);
        assert!(!render_text(&d).contains("tool sequence DIFFERS"));
        // Round attribution also preserves order in older records without IDs.
        for e in &mut a {
            if let Some(calls) = e.tool_calls.as_mut() {
                for c in calls {
                    c.tool_call_id = None;
                }
            }
        }
        let d = diff_events("a", "b", &a, &b);
        assert_eq!(d.a_tool_sequence, d.b_tool_sequence);
    }

    #[test]
    fn review_followup_standalone_error_is_retained_and_deduplicated() {
        let failed = json!({"tool_call_id":"failed","name":"bash","ok":false,
            "ms":1,"error":"failed","round":0});
        let error = evt(json!({"type":"tool_call_error","ts":"t","session_id":"a",
            "turn":1,"metadata":{"run_id":"root"},"tool_calls":[failed.clone()]}));
        let round = evt(json!({"type":"llm_round","ts":"t","session_id":"a",
            "turn":1,"metadata":{"run_id":"root"},"tool_calls":[failed.clone()]}));
        let terminal = evt(json!({"type":"turn_error","ts":"t","session_id":"a",
            "turn":1,"metadata":{"run_id":"root"},"tool_count":1,"tool_calls":[failed]}));
        for a in [
            vec![error.clone()],
            vec![error.clone(), round.clone()],
            vec![error.clone(), round.clone(), terminal.clone()],
            vec![error, terminal],
        ] {
            let d = diff_events("a", "b", &a, &[]);
            assert_eq!(d.a_tool_sequence, vec!["bash"]);
            assert_eq!(d.a_tool_calls_executed, 1);
            assert!(d.a_tool_sequence_complete);
        }
    }

    #[test]
    fn review_followup_partial_terminal_does_not_overwrite_error() {
        let a = vec![
            evt(
                json!({"type":"tool_call_error","ts":"t","session_id":"a","turn":1,
                "metadata":{"run_id":"root"},"tool_calls":[
                    {"tool_call_id":"f","name":"bash","ok":false,"ms":0}]}),
            ),
            evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
                "metadata":{"run_id":"root"},"tool_count":2,"tool_calls":[
                    {"tool_call_id":"s","name":"read_file","ok":true,"ms":0}]})),
        ];
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["bash", "read_file"]);
        assert_eq!(d.a_tool_calls_executed, 2);
        assert!(d.a_tool_sequence_complete);
    }

    #[test]
    fn indexed_reconciliation_preserves_large_journal_activity() {
        // 8,000 turns x (1 round + 20 trace events + 1 terminal): the old
        // per-round journal rescan took seconds here.
        let mut a = Vec::new();
        for turn in 1..=8000u32 {
            a.push(rd(turn, "root", "read_file"));
            for _ in 0..20 {
                a.push(evt(
                    json!({"type":"trace_span","ts":"t","session_id":"a","turn":turn}),
                ));
            }
            a.push(evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":turn,
                "tool_count":1,"metadata":{"run_id":"root"}}),
            ));
        }
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_calls_executed, 8000);
        assert!(d.a_tool_sequence_complete);
        assert_eq!(d.a_tool_sequence.len(), 8000);
    }

    #[test]
    fn executed_count_and_completeness_are_exposed_in_json_fields() {
        let a = vec![evt(json!({
            "type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":3,
            "tool_calls":[{"name":"Read","ok":true,"ms":0}]
        }))];
        let b = vec![evt(
            json!({"type":"turn","ts":"t","session_id":"b","turn":1,"tool_count":0}),
        )];
        let d = diff_events("a", "b", &a, &b);
        assert_eq!(d.a_tool_calls_executed, 3);
        assert!(
            !d.a_tool_sequence_complete,
            "1 record cannot cover 3 executed calls"
        );
        assert_eq!(d.b_tool_calls_executed, 0);
        assert!(
            d.b_tool_sequence_complete,
            "a turn with no tool calls is complete"
        );
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["a_tool_calls_executed"], 3);
        assert_eq!(json["a_tool_sequence_complete"], false);
        assert_eq!(json["b_tool_calls_executed"], 0);
        assert_eq!(json["b_tool_sequence_complete"], true);
        let txt = render_text(&d);
        assert!(txt.contains("tool sequence B (0 calls): []"), "{txt}");
        assert!(
            txt.contains("executed tool-call count DIFFERS (3 → 0)"),
            "{txt}"
        );
    }

    #[test]
    fn final_text_presence_is_reported_independently() {
        let a = vec![evt(
            json!({"type":"turn","ts":"t","session_id":"a","assistant_output":"done"}),
        )];
        // B has a turn but no assistant output — crash / empty run.
        let b = vec![evt(json!({"type":"turn","ts":"t","session_id":"b"}))];
        let d = diff_events("a", "b", &a, &b);
        assert!(d.a_has_final_text);
        assert!(!d.b_has_final_text);
    }

    #[test]
    fn render_text_shows_token_arrows_and_sequence_diff() {
        let a = vec![evt(
            json!({"type":"turn","ts":"t","session_id":"a","tokens_in":100,"tokens_out":10,"assistant_output":"x"}),
        )];
        let b = vec![evt(
            json!({"type":"turn","ts":"t","session_id":"b","tokens_in":120,"tokens_out":15,"assistant_output":"x"}),
        )];
        let d = diff_events("a", "b", &a, &b);
        let txt = render_text(&d);
        assert!(txt.contains("100 → 120"));
        assert!(txt.contains("(Δ 20)"));
        assert!(txt.contains("both runs produced output"));
    }

    #[test]
    fn token_delta_saturates_on_large_u64_instead_of_wrapping() {
        // u64 values above i64::MAX must not wrap to negative on cast.
        let big = (i64::MAX as u64) + 1000;
        let a = vec![evt(
            json!({"type":"turn","ts":"t","session_id":"a","tokens_in":big,"tokens_out":0}),
        )];
        let b = vec![evt(
            json!({"type":"turn","ts":"t","session_id":"b","tokens_in":0,"tokens_out":0}),
        )];
        let d = diff_events("a", "b", &a, &b);
        // Delta should be clamped, not wrapped to a positive number.
        assert!(
            d.token_deltas.prompt <= 0,
            "prompt delta must not wrap positive; got {}",
            d.token_deltas.prompt
        );
    }

    #[test]
    fn render_text_says_no_differences_when_events_match() {
        let a = vec![evt(json!({"type":"turn","ts":"t","session_id":"a"}))];
        let b = vec![evt(json!({"type":"turn","ts":"t","session_id":"b"}))];
        let d = diff_events("a", "b", &a, &b);
        let txt = render_text(&d);
        assert!(txt.contains("(no differences)"));
    }

    #[test]
    fn has_final_text_checks_last_event_not_any() {
        // A run that produced output early but crashed (last event has
        // no assistant_output) should report false — the "final" text
        // is what the user sees, not an intermediate turn.
        let a = vec![
            evt(
                json!({"type":"turn","ts":"t1","session_id":"a","assistant_output":"early output"}),
            ),
            evt(json!({"type":"turn","ts":"t2","session_id":"a"})),
        ];
        let b: Vec<JournalEvent> = vec![];
        let d = diff_events("a", "b", &a, &b);
        assert!(
            !d.a_has_final_text,
            "has_final_text must check the LAST event with potential output, \
             not any event. A run whose last turn has no output crashed."
        );
    }

    #[test]
    fn run_diff_warns_when_both_sessions_resolve_to_same_id() {
        let d = diff_events("sess-X", "sess-X", &[], &[]);
        assert_eq!(d.a_session, d.b_session, "sanity: sessions are the same");
        let txt = render_text(&d);
        assert!(
            txt.contains("warning") || txt.contains("Warning") || txt.contains("same session"),
            "render_text must warn when A and B are the same session; got:\n{txt}"
        );
    }
}

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

#[derive(Default)]
struct InvocationTerminals {
    positions: Vec<usize>,
    first_execution_terminal: Option<usize>,
}

/// Terminal positions indexed by explicit attempt identity.
#[derive(Default)]
struct TerminalIndex<'a> {
    by_run: HashMap<(u32, &'a str), Vec<usize>>,
    by_invocation: HashMap<CallKey<'a>, InvocationTerminals>,
    has_unscoped_terminal: bool,
}

impl<'a> TerminalIndex<'a> {
    fn new(events: &'a [JournalEvent]) -> Self {
        let mut index = Self::default();
        for (position, event) in events.iter().enumerate().filter(|(_, e)| is_terminal(e)) {
            if let (Some(turn), Some(run)) = (event.turn, run_id_of(event)) {
                index.by_run.entry((turn, run)).or_default().push(position);
            } else {
                index.has_unscoped_terminal = true;
            }
            for call in event.tool_calls.iter().flatten() {
                if let Some(key) = call_key(event, call) {
                    index
                        .by_invocation
                        .entry(key)
                        .or_default()
                        .positions
                        .push(position);
                }
            }
        }
        index
    }

    /// Aggregate buckets retain their counts, but a known invocation shared
    /// across them cannot certify disjoint execution coverage.
    fn execution_overlaps(&mut self, key: CallKey<'a>, terminal: usize) -> bool {
        let entry = self.by_invocation.entry(key).or_default();
        *entry.first_execution_terminal.get_or_insert(terminal) != terminal
    }

    /// Late observations still join their attempt. Prefer its first following
    /// terminal, then its nearest preceding terminal; turn alone is insufficient.
    fn owner(&self, event: &JournalEvent, call: &ToolCallRecord, position: usize) -> Option<usize> {
        let positions = call_key(event, call)
            .and_then(|key| self.by_invocation.get(key))
            .map(|entry| &entry.positions)
            .filter(|positions| !positions.is_empty())
            .or_else(|| self.by_run.get(&(event.turn?, run_id_of(event)?)))?;
        let after = positions.partition_point(|&i| i <= position);
        positions
            .get(after)
            .or_else(|| {
                after
                    .checked_sub(1)
                    .and_then(|before| positions.get(before))
            })
            .copied()
    }
}

/// Per-call observations, including callback results with uncertain delivery.
fn observed_calls(e: &JournalEvent) -> Option<&Vec<ToolCallRecord>> {
    matches!(
        e.event_type,
        JournalEventType::LlmRound | JournalEventType::ToolCallError | JournalEventType::SyncMarker
    )
    .then_some(e.tool_calls.as_ref())
    .flatten()
    .filter(|calls| !calls.is_empty())
}

type CallKey<'a> = &'a astra_turn_types::ToolInvocationIdentity;
type RoundCallIndex<'a> = HashMap<CallKey<'a>, (usize, usize)>;

/// Equality comes from the canonical invocation owner, not provider IDs or
/// event annotations. Terminal summaries may contain calls from earlier runs.
fn call_key<'a>(event: &JournalEvent, call: &'a ToolCallRecord) -> Option<CallKey<'a>> {
    let identity = call.invocation_identity.as_ref()?;
    if event.session_id.as_deref() != Some(identity.session_id.as_str())
        || call.tool_call_id.as_deref() != Some(identity.invocation_id.as_str())
        || (!is_terminal(event) && run_id_of(event).is_some_and(|run| run != identity.run_id))
    {
        return None;
    }
    Some(identity)
}

fn compatible_call(a: &ToolCallRecord, b: &ToolCallRecord) -> bool {
    a.name == b.name && a.was_executed() == b.was_executed()
}

/// A matching round owns call order, independently of error persistence order.
fn normalized_error<'a>(
    events: &'a [JournalEvent],
    terminals: &TerminalIndex<'_>,
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
    let &(round, offset) = round_positions.get(&call_key(event, call)?)?;
    let representative = &events[round].tool_calls.as_ref()?[offset];
    if !compatible_call(call, representative)
        || terminals.owner(event, call, event_idx)
            != terminals.owner(&events[round], representative, round)
    {
        return None;
    }
    Some(((round, offset), representative))
}

fn tool_activity(events: &[JournalEvent]) -> ToolActivity {
    // Terminals own aggregate counts. Only explicit attempt/call identities
    // reconcile projections; unproven or contradictory execution evidence is
    // retained with incomplete coverage, never silently consumed by an aggregate.
    let mut out = ToolActivity {
        sequence: Vec::new(),
        executed: 0,
        complete: true,
    };
    let mut index = TerminalIndex::new(events);
    let mut owned: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    let mut slots: BTreeMap<(usize, usize, usize), &ToolCallRecord> = BTreeMap::new();
    let mut orphan_calls: HashMap<CallKey<'_>, &ToolCallRecord> = HashMap::new();
    let mut round_positions = RoundCallIndex::new();
    if events
        .iter()
        .any(|event| event.event_type == JournalEventType::ToolCallError)
    {
        for (idx, event) in events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.event_type == JournalEventType::LlmRound)
        {
            for (offset, call) in event.tool_calls.iter().flatten().enumerate() {
                if let Some(key) = call_key(event, call) {
                    round_positions.entry(key).or_insert((idx, offset));
                }
            }
        }
    }
    for (idx, event) in events.iter().enumerate() {
        let Some(calls) = observed_calls(event) else {
            continue;
        };
        let normalized = normalized_error(events, &index, &round_positions, idx);
        for (offset, call) in calls.iter().enumerate() {
            if let Some(terminal) = index.owner(event, call, idx) {
                owned.entry(terminal).or_default().push((idx, offset));
                continue;
            }
            let ((position, within), call) = normalized.unwrap_or(((idx, offset), call));
            if let Some(key) = call_key(event, call) {
                if let Some(previous) = orphan_calls.get(&key) {
                    if compatible_call(previous, call) {
                        continue;
                    }
                    out.complete = false;
                } else {
                    orphan_calls.insert(key, call);
                }
            } else if call.was_executed() {
                out.complete = false;
            }
            if call.was_executed() && index.has_unscoped_terminal {
                out.complete = false;
            }
            out.executed = out.executed.saturating_add(u64::from(call.was_executed()));
            slots.insert((position, within, 0), call);
        }
    }
    for (idx, event) in events.iter().enumerate().filter(|(_, e)| is_terminal(e)) {
        let rounds = owned.remove(&idx).unwrap_or_default();
        let mut locations = HashMap::new();
        for &(r, offset) in &rounds {
            let call = &events[r].tool_calls.as_ref().unwrap()[offset];
            let (position, call) = normalized_error(events, &index, &round_positions, r)
                .unwrap_or(((r, offset), call));
            if let Some(key) = call_key(&events[r], call) {
                locations.entry(key).or_insert((position, call));
            }
        }
        let mut seen: HashMap<CallKey<'_>, &ToolCallRecord> = HashMap::new();
        let mut detailed = 0;
        let mut unidentified_terminal_execution = false;
        let mut unlocated_execution_seen = false;
        let mut relocated_after_terminal = false;
        for (offset, call) in event.tool_calls.iter().flatten().enumerate() {
            let key = call_key(event, call);
            if key.is_none() && call.was_executed() {
                unidentified_terminal_execution = true;
            }
            if let Some(key) = key {
                if let Some(previous) = seen.get(&key) {
                    if compatible_call(previous, call) {
                        continue;
                    }
                    out.complete = false;
                } else {
                    seen.insert(key, call);
                }
            }
            if call.invocation_identity.is_some() && key.is_none() {
                out.complete = false;
            }
            if call.was_executed()
                && key.is_some_and(|key| {
                    index.execution_overlaps(key, idx)
                        || orphan_calls
                            .get(&key)
                            .is_some_and(|call| call.was_executed())
                })
            {
                out.complete = false;
            }
            let matched_position = key
                .and_then(|key| locations.get(&key))
                .filter(|(_, observed)| compatible_call(call, observed))
                .map(|(position, _)| *position);
            if call.was_executed() {
                if let Some((position, _)) = matched_position {
                    if position < idx && unlocated_execution_seen {
                        out.complete = false;
                    }
                    relocated_after_terminal |= position > idx;
                } else {
                    if relocated_after_terminal {
                        out.complete = false;
                    }
                    unlocated_execution_seen = true;
                }
            }
            let (position, within) = matched_position.unwrap_or((idx, offset));
            slots.insert((position, within, offset + 1), call);
            detailed += u64::from(call.was_executed());
        }
        for &(r, offset) in &rounds {
            let call = &events[r].tool_calls.as_ref().unwrap()[offset];
            let ((position, within), call) = normalized_error(events, &index, &round_positions, r)
                .unwrap_or(((r, offset), call));
            let key = call_key(&events[r], call);
            let uncertain = if let Some(key) = key {
                if let Some(previous) = seen.get(&key) {
                    if compatible_call(previous, call) {
                        continue;
                    }
                    true
                } else {
                    seen.insert(key, call);
                    // A new identity does not prove disjoint coverage from
                    // terminal detail whose invocation identity is unavailable.
                    unidentified_terminal_execution && call.was_executed()
                }
            } else {
                true
            };
            if call.was_executed()
                && key.is_some_and(|key| {
                    index.execution_overlaps(key, idx)
                        || orphan_calls
                            .get(&key)
                            .is_some_and(|call| call.was_executed())
                })
            {
                out.complete = false;
            }
            slots.insert((position, within, 0), call);
            if uncertain {
                if key.is_some() {
                    out.complete = false;
                }
                // These observations cannot be certified as covered by the
                // terminal aggregate, even when it reports zero executions.
                if call.was_executed() {
                    out.complete = false;
                    out.executed = out.executed.saturating_add(1);
                }
            } else {
                detailed += u64::from(call.was_executed());
            }
        }
        let executed = event.tool_count.map(u64::from).unwrap_or(detailed);
        if detailed != executed {
            out.complete = false;
        }
        out.executed = out.executed.saturating_add(executed);
    }
    out.sequence = slots
        .into_values()
        .filter(|call| call.was_executed())
        .map(|call| call.name.clone())
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

    /// Complete producer fixtures. Tests for absent or conflicting identity
    /// use `evt` directly instead of this explicitly identified constructor.
    fn identified_evt(raw: serde_json::Value) -> JournalEvent {
        let mut event = evt(raw);
        if let (Some(sid), Some(run)) = (event.session_id.as_deref(), super::run_id_of(&event)) {
            let sid = sid.to_owned();
            let run = run.to_owned();
            for call in event.tool_calls.iter_mut().flatten() {
                if call.invocation_identity.is_none() {
                    call.invocation_identity = call.tool_call_id.as_ref().and_then(|id| {
                        astra_turn_types::ToolInvocationIdentity::new(
                            "owner", &sid, &run, "chain", id,
                        )
                        .ok()
                    });
                }
            }
        }
        event
    }

    fn identify(call: &mut super::ToolCallRecord, session: &str, run: &str) {
        call.invocation_identity = call.tool_call_id.as_ref().and_then(|id| {
            astra_turn_types::ToolInvocationIdentity::new("owner", session, run, "chain", id).ok()
        });
    }

    #[test]
    fn canonical_chains_distinguish_executions_and_replay_ignores_round() {
        let error = |chain: &str, round: u32| {
            identified_evt(json!({
                "type":"tool_call_error", "ts":"t", "session_id":"a", "turn":1,
                "producer_scope":{"run_id":"run"}, "tool_calls":[{
                    "tool_call_id":"call", "name":"bash", "ok":false, "ms":1, "round":round,
                    "invocation_identity":{"user_id":"owner", "session_id":"a", "run_id":"run",
                        "turn_chain_id":chain, "invocation_id":"call"}
                }]
            }))
        };
        for (chain, expected) in [("guided", 2), ("original", 1)] {
            let d = diff_events("a", "b", &[error("original", 0), error(chain, 1)], &[]);
            assert_eq!(d.a_tool_calls_executed, expected);
            assert_eq!(d.a_tool_sequence.len() as u64, expected);
            assert!(d.a_tool_sequence_complete);
        }
    }

    #[test]
    fn canonical_identity_survives_writer_and_terminal_summary_projection() {
        use astra_services::session_journal::{JournalDirGuard, JournalWriter, ToolCallRecord};
        let dir = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(dir.path());
        let sid = uuid::Uuid::new_v4().to_string();
        let empty = uuid::Uuid::new_v4().to_string();
        JournalWriter::new(&empty)
            .unwrap()
            .append(&JournalEvent::session_start(Some(&empty), None))
            .unwrap();
        let writer = JournalWriter::new(&sid).unwrap();
        let mut calls = Vec::new();
        for chain in ["original", "guided"] {
            let identity =
                astra_turn_types::ToolInvocationIdentity::new("owner", &sid, "run", chain, "call")
                    .unwrap();
            let record = ToolCallRecord {
                name: "bash".into(),
                ok: false,
                tool_call_id: Some("call".into()),
                invocation_identity: Some(identity),
                ..Default::default()
            };
            writer
                .append(
                    &JournalEvent::tool_call_error(Some(&sid), 1, "bash", "exit 1", record.clone())
                        .with_producer_scope(Some("run")),
                )
                .unwrap();
            calls.push(record);
        }
        assert_ne!(
            calls[0].invocation_identity.as_ref().unwrap().storage_key(),
            calls[1].invocation_identity.as_ref().unwrap().storage_key()
        );
        let mut terminal = JournalEvent::turn(Some(&sid), 1, None, "run", "done", 2, 0, 0, 0)
            .with_producer_scope(Some("summary-run"));
        terminal.tool_calls = Some(calls);
        writer.append(&terminal).unwrap();
        let d = super::compute_diff(&sid, &empty).unwrap();
        assert_eq!(d.a_tool_sequence, ["bash", "bash"]);
        assert_eq!(d.a_tool_calls_executed, 2);
        assert!(d.a_tool_sequence_complete);
    }

    #[test]
    fn partial_or_conflicting_identity_is_retained_without_replay_inference() {
        let raw = json!({"type":"tool_call_error", "ts":"t", "session_id":"a", "turn":1,
            "producer_scope":{"run_id":"run"}, "tool_calls":[{"name":"bash", "ok":false,
                "ms":1, "tool_call_id":"call", "invocation_identity":{"user_id":"owner",
                "session_id":"a", "run_id":"run", "turn_chain_id":"chain", "invocation_id":"call"}}]});
        for field in [
            "user_id",
            "session_id",
            "run_id",
            "turn_chain_id",
            "invocation_id",
        ] {
            for value in ["", " ", "different"] {
                let mut changed = raw.clone();
                changed["tool_calls"][0]["invocation_identity"][field] = json!(value);
                let d = diff_events("a", "b", &[evt(raw.clone()), evt(changed)], &[]);
                assert_eq!(d.a_tool_calls_executed, 2, "{field}={value:?}");
                assert_eq!(d.a_tool_sequence, ["bash", "bash"]);
                let conflict = ["session_id", "run_id", "invocation_id"].contains(&field);
                assert_eq!(
                    d.a_tool_sequence_complete,
                    value == "different" && !conflict
                );
            }
        }
        let mut missing = raw;
        missing["tool_calls"][0]
            .as_object_mut()
            .unwrap()
            .remove("invocation_identity");
        let d = diff_events("a", "b", &[evt(missing.clone()), evt(missing)], &[]);
        assert_eq!(d.a_tool_calls_executed, 2);
        assert!(!d.a_tool_sequence_complete);
    }

    #[test]
    fn canonical_identity_cannot_certify_disjoint_terminal_summaries() {
        use astra_services::session_journal::{
            JournalDirGuard, JournalWriter, ToolCallDisposition, ToolCallRecord,
        };
        let dir = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(dir.path());
        let empty = uuid::Uuid::new_v4().to_string();
        JournalWriter::new(&empty)
            .unwrap()
            .append(&JournalEvent::session_start(Some(&empty), None))
            .unwrap();
        #[derive(Debug)]
        enum Shape {
            Details,
            Orphan,
            Aggregates,
        }
        for (shape, first_executed, second_chain, second_name, complete) in [
            (Shape::Details, true, "chain", "bash", false),
            (Shape::Details, true, "chain", "read_file", false),
            (Shape::Details, true, "next-chain", "bash", true),
            (Shape::Details, false, "chain", "bash", true),
            (Shape::Orphan, true, "chain", "bash", false),
            (Shape::Orphan, true, "next-chain", "bash", true),
            (Shape::Aggregates, true, "chain", "bash", false),
            (Shape::Aggregates, true, "next-chain", "bash", true),
            (Shape::Orphan, false, "chain", "bash", true),
        ] {
            let label = format!("{shape:?}/{first_executed}/{second_chain}/{second_name}");
            let expected_count = if first_executed { 2 } else { 1 };
            let sid = uuid::Uuid::new_v4().to_string();
            let writer = JournalWriter::new(&sid).unwrap();
            let record = |chain: &str, name: &str, executed: bool| ToolCallRecord {
                name: name.into(),
                ok: true,
                tool_call_id: Some("call".into()),
                invocation_identity: Some(
                    astra_turn_types::ToolInvocationIdentity::new(
                        "owner", &sid, "run", chain, "call",
                    )
                    .unwrap(),
                ),
                disposition: (!executed).then_some(ToolCallDisposition::Rejected),
                ..Default::default()
            };
            for (index, call) in [
                record("chain", "bash", first_executed),
                record(second_chain, second_name, true),
            ]
            .into_iter()
            .enumerate()
            {
                let turn = index as u32 + 1;
                if !matches!(shape, Shape::Details) {
                    writer.append(&evt(json!({
                        "type":"llm_round", "ts":format!("2026-10-20T00:00:0{}Z", index * 2 + 1),
                        "session_id":sid, "turn":turn, "producer_scope":{"run_id":"run"},
                        "tool_calls":[call.clone()]
                    }))).unwrap();
                }
                if !matches!(shape, Shape::Orphan) || index == 1 {
                    let run = if matches!(shape, Shape::Details) && index == 1 {
                        "summary-run"
                    } else {
                        "run"
                    };
                    let mut terminal = JournalEvent::turn(
                        Some(&sid),
                        turn,
                        None,
                        "user",
                        "done",
                        u32::from(call.was_executed()),
                        0,
                        0,
                        1,
                    )
                    .with_producer_scope(Some(run));
                    terminal.ts = format!("2026-10-20T00:00:0{}Z", index * 2 + 2);
                    if matches!(shape, Shape::Details) {
                        terminal.tool_calls = Some(vec![call]);
                    }
                    writer.append(&terminal).unwrap();
                }
            }
            let diff = super::compute_diff(&sid, &empty).unwrap();
            assert_eq!(diff.a_tool_calls_executed, expected_count, "{label}");
            assert_eq!(diff.a_tool_sequence_complete, complete, "{label}");
            let names = if first_executed {
                vec!["bash", second_name]
            } else {
                vec![second_name]
            };
            assert_eq!(diff.a_tool_sequence, names, "{label}");
            assert_eq!(
                serde_json::to_value(&diff).unwrap()["a_tool_sequence_complete"],
                complete,
                "{label}"
            );
            if !complete {
                let text = render_text(&diff);
                assert!(text.contains("sequences not comparable"), "{label}: {text}");
                assert!(!text.contains("→ tool sequence DIFFERS"), "{label}: {text}");
            }
        }
    }

    #[test]
    fn partial_terminal_identity_does_not_prove_disjoint_round_coverage() {
        use astra_services::session_journal::{
            JournalDirGuard, JournalWriter, ToolCallDisposition, ToolCallRecord,
        };
        let dir = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(dir.path());
        let empty = uuid::Uuid::new_v4().to_string();
        JournalWriter::new(&empty)
            .unwrap()
            .append(&JournalEvent::session_start(Some(&empty), None))
            .unwrap();
        let known = |sid: &str, id: &str| ToolCallRecord {
            name: "bash".into(),
            ok: false,
            error: Some("exit 1".into()),
            tool_call_id: Some(id.into()),
            invocation_identity: Some(
                astra_turn_types::ToolInvocationIdentity::new("owner", sid, "run", "chain", id)
                    .unwrap(),
            ),
            ..Default::default()
        };
        let verify = |sid: &str,
                      observed: ToolCallRecord,
                      terminal_calls: Vec<ToolCallRecord>,
                      aggregate: u32,
                      expected: (u64, usize, bool)| {
            let round = evt(
                json!({"type":"llm_round", "ts":"2026-10-19T00:00:01Z", "session_id":sid,
                "turn":1, "producer_scope":{"run_id":"run"}, "tool_calls":[observed.clone()]}),
            );
            let mut error = JournalEvent::tool_call_error(Some(sid), 1, "bash", "failed", observed)
                .with_producer_scope(Some("run"));
            let mut terminal =
                JournalEvent::turn(Some(sid), 1, None, "run", "done", aggregate, 0, 0, 0)
                    .with_producer_scope(Some("run"));
            error.ts = "2026-10-19T00:00:00Z".into();
            terminal.ts = "2026-10-19T00:00:02Z".into();
            terminal.tool_calls = Some(terminal_calls);
            let writer = JournalWriter::new(sid).unwrap();
            // An uncertain but identified call contributes once, even when
            // error persistence and repeated round projections surround it.
            for event in [&error, &round, &round, &terminal] {
                writer.append(event).unwrap();
            }
            let d = super::compute_diff(sid, &empty).unwrap();
            assert_eq!(d.a_tool_calls_executed, expected.0);
            assert_eq!(d.a_tool_sequence, vec!["bash"; expected.1]);
            assert_eq!(d.a_tool_sequence_complete, expected.2);
            let encoded = serde_json::to_value(&d).unwrap();
            assert_eq!(encoded["a_tool_sequence_complete"], expected.2);
            if !expected.2 {
                assert!(render_text(&d).contains("unavailable/incomplete"));
            }
        };
        // Terminal detail may overlap an identified round: aggregate equality
        // cannot name the missing second execution. Controls prove disjointness
        // with full identity, or carry no executed terminal detail to overlap.
        for (id, identified, executed, aggregate, expected) in [
            (None, false, true, 0, (1, 2, false)),
            (None, false, true, 1, (2, 2, false)),
            (None, false, true, 2, (3, 2, false)),
            (Some(" "), false, true, 2, (3, 2, false)),
            (Some("call"), false, true, 2, (3, 2, false)),
            (None, true, true, 2, (3, 2, false)),
            (Some("call"), true, true, 2, (2, 1, false)),
            (Some("other"), true, true, 2, (2, 2, true)),
            (None, false, false, 1, (1, 1, true)),
        ] {
            let sid = uuid::Uuid::new_v4().to_string();
            let observed = known(&sid, "call");
            let mut partial = known(
                &sid,
                id.filter(|id| !id.trim().is_empty()).unwrap_or("call"),
            );
            partial.tool_call_id = id.map(str::to_owned);
            if !identified {
                partial.invocation_identity = None;
            }
            partial.disposition = Some(if executed {
                ToolCallDisposition::Executed
            } else {
                ToolCallDisposition::Rejected
            });
            verify(&sid, observed, vec![partial], aggregate, expected);
        }
        // A new nonexecuted external identity adds no execution coverage,
        // even beside unidentified executed terminal detail.
        for (terminal_executed, aggregate, expected) in
            [(true, 1, (1, 1, true)), (false, 0, (0, 0, true))]
        {
            let sid = uuid::Uuid::new_v4().to_string();
            let mut observed = known(&sid, "call");
            observed.disposition = Some(ToolCallDisposition::Reused);
            let mut unknown = known(&sid, "unknown");
            unknown.invocation_identity = None;
            if !terminal_executed {
                unknown.disposition = Some(ToolCallDisposition::Rejected);
            }
            verify(&sid, observed, vec![unknown], aggregate, expected);
        }
        // Unknown detail does not blanket-poison the terminal's own list.
        // Only a new external identity faces the unresolved overlap barrier.
        for (terminal_id, expected) in [("call", (2, 2, true)), ("other", (3, 3, false))] {
            let sid = uuid::Uuid::new_v4().to_string();
            let mut unknown = known(&sid, "unknown");
            unknown.invocation_identity = None;
            verify(
                &sid,
                known(&sid, "call"),
                vec![known(&sid, terminal_id), unknown],
                2,
                expected,
            );
        }
    }

    #[test]
    fn redundant_rounds_do_not_certify_reordered_terminal_detail() {
        use astra_services::session_journal::{
            JournalDirGuard, JournalWriter, ToolCallDisposition, ToolCallRecord,
        };
        let dir = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(dir.path());
        // Identity and observed position are separate facts. The first two
        // modes lack a position for read_file; the controls either locate both
        // calls or exclude read_file from the executed sequence.
        for (identified, locate_both, executed, reverse_round, duplicate, expected_complete) in [
            (false, false, true, false, false, [false, true, true, false]),
            (true, false, true, false, false, [false, true, true, false]),
            (true, true, true, false, false, [true; 4]),
            (false, false, false, false, false, [true; 4]),
            // Skipped canonical duplicates must not create ordering barriers.
            (true, false, true, false, true, [false, true, true, false]),
            // Positive canonical positions still own round-internal order.
            (true, true, true, true, false, [true; 4]),
        ] {
            for (case, (unlocated_first, late)) in
                [(true, false), (true, true), (false, false), (false, true)]
                    .into_iter()
                    .enumerate()
            {
                let write_session = |with_round: bool| {
                    let sid = uuid::Uuid::new_v4().to_string();
                    let identity = |id: &str| {
                        astra_turn_types::ToolInvocationIdentity::new(
                            "owner", &sid, "run", "chain", id,
                        )
                        .unwrap()
                    };
                    let unlocated = ToolCallRecord {
                        name: "read_file".into(),
                        ok: true,
                        disposition: Some(if executed {
                            ToolCallDisposition::Executed
                        } else {
                            ToolCallDisposition::Rejected
                        }),
                        tool_call_id: identified.then(|| "read".into()),
                        invocation_identity: identified.then(|| identity("read")),
                        ..Default::default()
                    };
                    let located = ToolCallRecord {
                        name: "grep".into(),
                        ok: false,
                        error: Some("failed".into()),
                        tool_call_id: Some("grep".into()),
                        invocation_identity: Some(identity("grep")),
                        ..Default::default()
                    };
                    let mut calls = if unlocated_first {
                        vec![unlocated, located.clone()]
                    } else {
                        vec![located.clone(), unlocated]
                    };
                    let mut round_calls = if locate_both {
                        calls.clone()
                    } else {
                        vec![located.clone()]
                    };
                    if reverse_round {
                        round_calls.reverse();
                    }
                    if duplicate {
                        calls.push(located);
                    }
                    let mut round = evt(json!({"type":"llm_round", "ts":"t", "session_id":sid,
                        "turn":1, "producer_scope":{"run_id":"run"}, "tool_calls":round_calls}));
                    let mut terminal = JournalEvent::turn(
                        Some(&sid),
                        1,
                        None,
                        "run",
                        "done",
                        if executed { 2 } else { 1 },
                        0,
                        0,
                        0,
                    )
                    .with_producer_scope(Some("run"));
                    round.ts = if late {
                        "2026-10-19T00:00:02Z"
                    } else {
                        "2026-10-19T00:00:00Z"
                    }
                    .into();
                    terminal.ts = "2026-10-19T00:00:01Z".into();
                    terminal.tool_calls = Some(calls);
                    let writer = JournalWriter::new(&sid).unwrap();
                    if with_round && !late {
                        writer.append(&round).unwrap();
                    }
                    writer.append(&terminal).unwrap();
                    if with_round && late {
                        writer.append(&round).unwrap();
                    }
                    sid
                };
                let observed = write_session(true);
                let control = write_session(false);
                let d = super::compute_diff(&observed, &control).unwrap();
                assert_eq!(d.a_tool_calls_executed, if executed { 2 } else { 1 });
                assert_eq!(d.a_tool_calls_executed, d.b_tool_calls_executed);
                assert!(d.b_tool_sequence_complete);
                assert_eq!(
                    d.a_tool_sequence_complete, expected_complete[case],
                    "identified={identified} locate_both={locate_both} executed={executed} first={unlocated_first} late={late}"
                );
                if d.a_tool_sequence_complete {
                    let mut expected_order = d.b_tool_sequence.clone();
                    if reverse_round {
                        expected_order.reverse();
                    }
                    assert_eq!(d.a_tool_sequence, expected_order);
                }
                let encoded = serde_json::to_value(&d).unwrap();
                assert_eq!(encoded["a_tool_sequence_complete"], expected_complete[case]);
                let mut observed_names = d.a_tool_sequence.clone();
                let mut control_names = d.b_tool_sequence.clone();
                observed_names.sort();
                control_names.sort();
                assert_eq!(observed_names, control_names);
                assert_eq!(
                    render_text(&d).contains("tool sequence DIFFERS"),
                    reverse_round
                );
            }
        }
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
            {"tool_call_id":"read","name":"Read","ok":true,"ms":0},
            {"tool_call_id":"grep","name":"Grep","ok":true,"ms":0},
        ]);
        let a = vec![
            identified_evt(
                json!({"type":"llm_round","ts":"t1","session_id":"a","turn":1,"producer_scope":{"run_id":"root"},
                "tool_calls":[{"tool_call_id":"read","name":"Read","ok":true,"ms":0}]}),
            ),
            identified_evt(
                json!({"type":"llm_round","ts":"t2","session_id":"a","turn":1,"producer_scope":{"run_id":"root"},
                "tool_calls":[{"tool_call_id":"grep","name":"Grep","ok":true,"ms":0}]}),
            ),
            identified_evt(
                json!({"type":"turn","ts":"t3","session_id":"a","turn":1,"producer_scope":{"run_id":"root"},
                "tool_count":2,"tool_calls":calls}),
            ),
        ];
        let d = diff_events("a", "a2", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["Read", "Grep"]);
    }

    fn rd(turn: u32, run: &str, name: &str) -> JournalEvent {
        identified_evt(
            json!({"type":"llm_round","ts":"t","session_id":"a","turn":turn,
            "metadata":{"run_id":run},"tool_calls":[{"tool_call_id":name,"name":name,"ok":true,"ms":0}]}),
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
            identified_evt(
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
            identified_evt(
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
            identified_evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":1,
                "metadata":{"run_id":"r1"},"tool_calls":[{"tool_call_id":"read_file","name":"read_file","ok":true,"ms":0}]}),
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
            identified_evt(
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
            identified_evt(json!({"type":"llm_round","ts":"t","session_id":"a",
                "producer_scope":{"run_id":"child","parent_run_id":"root","agent_id":"worker"},
                "tool_calls":[{"tool_call_id":"grep","name":"grep","ok":true,"ms":0}]})),
            identified_evt(
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
        a[2] = identified_evt(
            json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":1,
            "metadata":{"run_id":"root"},"tool_calls":[{"tool_call_id":"read_file","name":"read_file","ok":true,"ms":0}]}),
        );
        let d = diff_events("a", "b", &a, &[]);
        assert_eq!(d.a_tool_sequence, vec!["read_file", "grep"]);
        assert_eq!(d.a_tool_calls_executed, 2);
    }

    #[test]
    fn missing_scope_never_establishes_attempt_ownership() {
        let terminal = identified_evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
            "tool_count":1,"tool_calls":[{"tool_call_id":"read_file","name":"read_file","ok":true,"ms":0}]}));
        for (run, name) in [
            (Some("root"), "read_file"),
            (Some("child"), "grep"),
            (None, "grep"),
        ] {
            let mut round = rd(1, run.unwrap_or("root"), name);
            if run.is_none() {
                round.metadata = None;
            }
            let diff = diff_events("a", "b", &[round, terminal.clone()], &[]);
            assert_eq!(diff.a_tool_sequence, [name, "read_file"]);
            assert_eq!(diff.a_tool_calls_executed, 2);
            assert!(!diff.a_tool_sequence_complete);
        }
    }

    #[test]
    fn terminal_lookup_uses_exact_run_and_nearest_following_position() {
        let terminal = |run: &str| {
            identified_evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
            "producer_scope":{"run_id":run},"tool_count":1,"tool_calls":[
                {"tool_call_id":"read_file","name":"read_file","ok":true,"ms":0}]}))
        };
        let events = vec![
            terminal("root"),
            rd(1, "root", "read_file"),
            terminal("other"),
        ];
        let diff = diff_events("a", "b", &events, &[]);
        assert_eq!(diff.a_tool_sequence, ["read_file", "read_file"]);
        assert_eq!(diff.a_tool_calls_executed, 2);
        assert!(diff.a_tool_sequence_complete);
        // Different terminal positions in the same run/turn still delimit
        // observations; nearest following wins over the preceding terminal.
        let mut before = terminal("root");
        before.tool_calls = None;
        before.tool_count = Some(0);
        let diff = diff_events(
            "a",
            "b",
            &[before, rd(1, "root", "read_file"), terminal("root")],
            &[],
        );
        assert_eq!(diff.a_tool_sequence, ["read_file"]);
        assert_eq!(diff.a_tool_calls_executed, 1);
        assert!(diff.a_tool_sequence_complete);
    }

    async fn assert_unwitnessed_error_after_restore(detail: Option<bool>) {
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
        if let Some(with_detail) = detail {
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
                        "tool_call_id":"aborted-call","name":"read_file","ok":true,"ms":1
                    }))
                    .unwrap(),
                ]);
            }
            writer.append(&terminal).unwrap();
        } else {
            let second = serde_json::from_value(json!({
                "tool_call_id":"aborted-call","name":"read_file","ok":false,
                "ms":1,"error":"not found"
            }))
            .unwrap();
            writer
                .append(&JournalEvent::tool_call_error(
                    Some(&sid),
                    restored.turn_count + 1,
                    "read_file",
                    "not found",
                    second,
                ))
                .unwrap();
            assert_eq!(
                HybridRestoreService::local_only()
                    .restore_local_session(&sid)
                    .await
                    .unwrap()
                    .unwrap()
                    .turn_count,
                0
            );
        }
        let diff = super::compute_diff(&sid, &empty).unwrap();
        assert_eq!(diff.a_tool_calls_executed, 2);
        assert!(!diff.a_tool_sequence_complete);
        let expected = if detail != Some(false) {
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
        assert_unwitnessed_error_after_restore(Some(true)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unwitnessed_error_survives_restore_with_aggregate_later_turn() {
        assert_unwitnessed_error_after_restore(Some(false)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_interrupted_attempts_preserve_reused_provider_call_ids() {
        assert_unwitnessed_error_after_restore(None).await;
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
            let events = [identified_evt(
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
        let terminal = identified_evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
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
            let mut read: super::ToolCallRecord = serde_json::from_value(json!({
                "tool_call_id":"read","name":"read_file","round":0,
                "start_offset_ms":0,"ok":true,"ms":1
            }))
            .unwrap();
            let mut failed: super::ToolCallRecord = serde_json::from_value(json!({
                "tool_call_id":"failed","name":"bash","round":0,
                "start_offset_ms":10,"ok":false,"ms":1,"error":"exit 1"
            }))
            .unwrap();
            identify(&mut read, &sid, "root");
            identify(&mut failed, &sid, "root");
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
                event = if explicit_run {
                    event.with_run_id(Some("root"))
                } else {
                    event.with_producer_scope(Some("root"))
                };
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
    fn scoped_errors_reconcile_but_anonymous_observations_remain_uncertain() {
        let mut record: super::ToolCallRecord = serde_json::from_value(json!({
            "tool_call_id":"failed-bash","name":"bash","round":0,
            "ok":false,"ms":1,"error":"exit 1"
        }))
        .unwrap();
        identify(&mut record, "a", "root");
        // The producer preserves its run using the existing scope builder.
        let error = JournalEvent::tool_call_error(Some("a"), 1, "bash", "exit 1", record.clone())
            .with_producer_scope(Some("root"));
        let round = identified_evt(
            json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
            "producer_scope":{"run_id":"root"},"tool_calls":[record]}),
        );
        let terminal = identified_evt(
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
        for (error, retained) in [(missing, 2), (multiple, 2)] {
            let diff = diff_events("a", "b", &[round.clone(), error], &[]);
            assert_eq!(diff.a_tool_calls_executed, retained);
            assert!(!diff.a_tool_sequence_complete);
        }
        // Missing run identity on both projections is not a positive witness.
        // Keep uncertain error evidence separate from a runless round.
        let mut runless = round.clone();
        runless.producer_scope = None;
        runless.tool_calls.as_mut().unwrap()[0].invocation_identity = None;
        let error = JournalEvent::tool_call_error(
            Some("a"),
            1,
            "bash",
            "exit 1",
            runless.tool_calls.as_ref().unwrap()[0].clone(),
        );
        for (events, count) in [
            (vec![runless.clone(), error.clone(), error.clone()], 3),
            (vec![error, runless], 2),
        ] {
            let diff = diff_events("a", "b", &events, &[]);
            assert_eq!(diff.a_tool_calls_executed, count);
            assert!(!diff.a_tool_sequence_complete);
        }
        // Empty scopes are missing identity, including values persisted by
        // the generic metadata builder. Payload equality cannot prove replay.
        for blank in ["", "   "] {
            for typed in [false, true] {
                let mut error = JournalEvent::tool_call_error(
                    Some("a"),
                    1,
                    "bash",
                    "exit 1",
                    round.tool_calls.as_ref().unwrap()[0].clone(),
                )
                .with_run_id(Some(blank));
                error.tool_calls.as_mut().unwrap()[0].invocation_identity = None;
                if typed {
                    error.producer_scope =
                        Some(serde_json::from_value(json!({"run_id":blank})).unwrap());
                }
                let diff = diff_events("a", "b", &[error.clone(), error], &[]);
                assert_eq!(diff.a_tool_sequence, ["bash", "bash"]);
                assert_eq!(diff.a_tool_calls_executed, 2);
                assert!(!diff.a_tool_sequence_complete);
            }
        }
        // A reused provider ID in a distinct known run is not a duplicate.
        let mut other = round.clone();
        other.producer_scope.as_mut().unwrap().run_id = "other".to_string();
        for call in other.tool_calls.iter_mut().flatten() {
            identify(call, "a", "other");
        }
        let diff = diff_events("a", "b", &[round, other], &[]);
        assert_eq!(diff.a_tool_sequence, ["bash", "bash"]);
        assert_eq!(diff.a_tool_calls_executed, 2);
    }

    #[test]
    fn scoped_and_unscoped_child_errors_never_join_anonymous_root() {
        let mut record: super::ToolCallRecord = serde_json::from_value(json!({
            "tool_call_id":"child-failure","name":"bash","ok":false,"ms":1
        }))
        .unwrap();
        identify(&mut record, "a", "child");
        let error = JournalEvent::tool_call_error(Some("a"), 1, "bash", "failed", record.clone())
            .with_producer_scope(Some("child"));
        let child = identified_evt(
            json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
            "producer_scope":{"run_id":"child","parent_run_id":"root"},"tool_calls":[record]}),
        );
        let root = identified_evt(
            json!({"type":"turn","ts":"t","session_id":"a","turn":1,"tool_count":0}),
        );
        for a in [
            vec![error.clone(), child.clone(), root.clone()],
            vec![root.clone(), child.clone(), error.clone()],
        ] {
            let d = diff_events("a", "b", &a, &[]);
            assert_eq!(d.a_tool_sequence, ["bash"]);
            assert_eq!(d.a_tool_calls_executed, 1);
            assert!(!d.a_tool_sequence_complete);
        }
        let mut other = child.clone();
        other.producer_scope.as_mut().unwrap().run_id = "other".to_string();
        for call in other.tool_calls.iter_mut().flatten() {
            identify(call, "a", "other");
        }
        let mut anonymous = error;
        anonymous.producer_scope = None;
        anonymous.tool_calls.as_mut().unwrap()[0].invocation_identity = None;
        let d = diff_events("a", "b", &[anonymous, child, other, root], &[]);
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
            identified_evt(
                json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
                "metadata":{"run_id":"root"},"tool_calls":[call("a", "read_file", 0)]}),
            ),
            identified_evt(json!({"type":"llm_round","ts":"t","session_id":"a",
                "producer_scope":{"run_id":"child","parent_run_id":"root","agent_id":"worker"},
                "tool_calls":[call("c", "grep", 0)]})),
            identified_evt(
                json!({"type":"llm_round","ts":"t","session_id":"a","turn":1,
                "metadata":{"run_id":"root"},"tool_calls":[call("b", "bash", 1)]}),
            ),
            identified_evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
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
        // Without call IDs, projections cannot be proved redundant.
        for e in &mut a {
            if let Some(calls) = e.tool_calls.as_mut() {
                for c in calls {
                    c.tool_call_id = None;
                }
            }
        }
        let d = diff_events("a", "b", &a, &b);
        assert_eq!(
            d.a_tool_sequence,
            ["read_file", "grep", "bash", "read_file", "bash"]
        );
        assert_eq!(d.a_tool_calls_executed, 5);
        assert!(!d.a_tool_sequence_complete);
    }

    #[test]
    fn review_followup_standalone_error_is_retained_and_deduplicated() {
        let failed = json!({"tool_call_id":"failed","name":"bash","ok":false,
            "ms":1,"error":"failed","round":0});
        let error = identified_evt(json!({"type":"tool_call_error","ts":"t","session_id":"a",
            "turn":1,"metadata":{"run_id":"root"},"tool_calls":[failed.clone()]}));
        let round = identified_evt(json!({"type":"llm_round","ts":"t","session_id":"a",
            "turn":1,"metadata":{"run_id":"root"},"tool_calls":[failed.clone()]}));
        let terminal = identified_evt(json!({"type":"turn_error","ts":"t","session_id":"a",
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
    fn missing_or_conflicting_call_identity_never_erases_execution() {
        use astra_services::session_journal::ToolCallDisposition::{Executed, Rejected};
        for (run, id, terminal_name, disposition, count, complete) in [
            (None, Some("call"), "read_file", Executed, 2, false),
            (Some("root"), Some("call"), "read_file", Executed, 2, false),
            (Some("root"), Some("call"), "bash", Rejected, 1, false),
            (Some("root"), None, "bash", Rejected, 1, false),
            (Some("root"), Some("   "), "bash", Rejected, 1, false),
            (Some("root"), Some("call"), "bash", Executed, 1, true),
            (Some("other"), Some("call"), "bash", Executed, 2, true),
        ] {
            let mut record = serde_json::from_value(json!({
                "tool_call_id":id,"name":"bash","ok":false,"ms":1
            }))
            .unwrap();
            if let Some(run) = run {
                identify(&mut record, "a", run);
            }
            let error = JournalEvent::tool_call_error(Some("a"), 1, "bash", "failed", record)
                .with_producer_scope(run);
            let terminal = identified_evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
            "producer_scope":{"run_id":"root"},
            "tool_count":u32::from(disposition == Executed),"tool_calls":[{
                "tool_call_id":"call","name":terminal_name,"ok":true,"ms":1,
                "disposition":disposition
            }]}));
            let diff = diff_events("a", "b", &[error, terminal], &[]);
            assert_eq!(
                diff.a_tool_calls_executed, count,
                "run={run:?} id={id:?} terminal={terminal_name}"
            );
            assert_eq!(diff.a_tool_sequence_complete, complete);
            assert!(diff.a_tool_sequence.iter().any(|name| name == "bash"));
            let encoded = serde_json::to_value(&diff).unwrap();
            assert_eq!(encoded["a_tool_calls_executed"], count);
            assert_eq!(encoded["a_tool_sequence_complete"], complete);
        }
    }

    #[test]
    fn review_followup_partial_terminal_does_not_overwrite_error() {
        let a = vec![
            identified_evt(
                json!({"type":"tool_call_error","ts":"t","session_id":"a","turn":1,
                "metadata":{"run_id":"root"},"tool_calls":[
                    {"tool_call_id":"f","name":"bash","ok":false,"ms":0}]}),
            ),
            identified_evt(json!({"type":"turn","ts":"t","session_id":"a","turn":1,
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
            let run = format!("run-{turn}");
            a.push(rd(turn, &run, "read_file"));
            for _ in 0..20 {
                a.push(identified_evt(
                    json!({"type":"trace_span","ts":"t","session_id":"a","turn":turn}),
                ));
            }
            a.push(identified_evt(
                json!({"type":"turn","ts":"t","session_id":"a","turn":turn,
                "tool_count":1,"metadata":{"run_id":run}}),
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

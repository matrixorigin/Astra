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

use std::collections::BTreeMap;

use serde::Serialize;

use astra_services::session_journal::{self, JournalEvent, JournalEventType};

use crate::cli_args;
use crate::journal_digest;

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
    /// Aggregate token totals.
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

    JournalDiff {
        a_session: a_session.to_string(),
        b_session: b_session.to_string(),
        a_event_counts: a_counts,
        b_event_counts: b_counts,
        only_in_a,
        only_in_b,
        count_deltas,
        a_tool_sequence: tool_sequence(a_events),
        b_tool_sequence: tool_sequence(b_events),
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
    let mut t = TokenTotals::default();
    for e in events {
        if let Some(p) = e.tokens_in {
            t.prompt = t.prompt.saturating_add(p);
        }
        if let Some(c) = e.tokens_out {
            t.completion = t.completion.saturating_add(c);
        }
    }
    t.total = t.prompt.saturating_add(t.completion);
    t
}

fn tool_sequence(events: &[JournalEvent]) -> Vec<String> {
    // Walk `JournalEvent.tool_calls` (set on Turn / LlmRound events) in
    // journal order. The per-event vec preserves within-turn ordering;
    // cross-event traversal preserves turn ordering. Union of both gives
    // the full tool-call sequence a reviewer sees when reading the run
    // top-to-bottom.
    let mut out = Vec::new();
    for e in events {
        if let Some(calls) = e.tool_calls.as_ref() {
            for c in calls {
                out.push(c.name.clone());
            }
        }
    }
    out
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
    s.push_str(&format!(
        "tool sequence A ({} calls): {:?}\n",
        diff.a_tool_sequence.len(),
        diff.a_tool_sequence
    ));
    s.push_str(&format!(
        "tool sequence B ({} calls): {:?}\n",
        diff.b_tool_sequence.len(),
        diff.b_tool_sequence
    ));
    if diff.a_tool_sequence != diff.b_tool_sequence {
        s.push_str("  → tool sequence DIFFERS\n");
    }

    s
}

pub fn run_diff(args: &cli_args::JournalDiffArgs) -> Result<(), String> {
    let a = journal_digest::resolve_session_for_digest(Some(&args.a), None)?;
    let b = journal_digest::resolve_session_for_digest(Some(&args.b), None)?;
    let diff = compute_diff(&a, &b)?;
    let format = args.format.trim().to_ascii_lowercase();
    match format.as_str() {
        "" | "text" | "txt" => {
            print!("{}", render_text(&diff));
            Ok(())
        }
        "json" => {
            println!(
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
    use super::*;
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

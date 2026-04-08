//! Local session journal digest for `astra journal digest` and tooling.

use astra_services::session_journal::{self, JournalEventType};
use serde::Serialize;
use serde_json::json;

pub const SCHEMA_VERSION: &str = "astra-journal-digest-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestFocus {
    All,
    Summary,
}

pub fn parse_focus(raw: Option<&str>) -> Result<DigestFocus, String> {
    match raw
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "all" => Ok(DigestFocus::All),
        "summary" => Ok(DigestFocus::Summary),
        other => Err(format!(
            "invalid --focus '{other}' (expected all or summary)"
        )),
    }
}

/// `positional` is the optional CLI positional; `long_session` is `--session`.
pub fn resolve_session_for_digest(
    positional: Option<&str>,
    long_session: Option<&str>,
) -> Result<String, String> {
    let query = positional
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| long_session.map(str::trim).filter(|s| !s.is_empty()));
    match query {
        None => session_journal::list_sessions_by_time(1)
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| "no local session journals found".to_string()),
        Some(q)
            if q.eq_ignore_ascii_case("last")
                || q.eq_ignore_ascii_case("previous")
                || q.eq_ignore_ascii_case("recent") =>
        {
            session_journal::list_sessions_by_time(1)
                .map_err(|e| e.to_string())?
                .into_iter()
                .next()
                .ok_or_else(|| "no local session journals found".to_string())
        }
        Some(q) => session_journal::resolve_session_id(q).map_err(|e| e.to_string()),
    }
}

#[derive(Serialize)]
pub struct JournalDigest {
    pub schema_version: &'static str,
    pub session_id: String,
    pub journal_file: String,
    /// Non-empty lines in the JSONL file.
    pub journal_lines_non_empty: usize,
    /// Lines that were non-empty but not valid `JournalEvent` JSON.
    pub journal_lines_malformed: usize,
    pub aggregates: Aggregates,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<TurnRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub compaction_events: Vec<SideEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stalls: Vec<SideEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub turn_errors: Vec<TurnErrRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_errors: Vec<SideEvent>,
}

#[derive(Serialize)]
pub struct Aggregates {
    pub turn_count: usize,
    pub turn_error_count: usize,
    pub compact_count: usize,
    pub stall_count: usize,
    pub error_event_count: usize,
    pub session_start_count: usize,
    pub session_end_count: usize,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_duration_ms: u64,
    pub total_tool_calls: u64,
    pub tool_calls_failed: u64,
    pub avg_tokens_in: f64,
    pub avg_tokens_out: f64,
    pub avg_duration_ms: f64,
}

#[derive(Serialize)]
pub struct TurnRow {
    pub seq: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u32>,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_domain_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_learn_skipped_no_domain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memoria_ms: Option<u64>,
    pub tools_selected_count: usize,
    pub tools_used_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selected_skills: Vec<String>,
    pub tool_calls_ok: u32,
    pub tool_calls_fail: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user_input_preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_pressure: Option<f64>,
}

#[derive(Serialize)]
pub struct SideEvent {
    pub ts: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    pub detail: serde_json::Value,
}

#[derive(Serialize)]
pub struct TurnErrRow {
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    pub error: String,
}

fn preview(s: Option<&String>, max: usize) -> String {
    let Some(s) = s.map(String::as_str) else {
        return String::new();
    };
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut out = String::new();
    for (i, ch) in t.chars().enumerate() {
        if i >= max.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn tool_call_counts(calls: Option<&Vec<session_journal::ToolCallRecord>>) -> (u32, u32) {
    let Some(calls) = calls else {
        return (0, 0);
    };
    let mut ok = 0u32;
    let mut fail = 0u32;
    for c in calls {
        if c.ok {
            ok += 1;
        } else {
            fail += 1;
        }
    }
    (ok, fail)
}

pub fn build_digest(session_id: &str, focus: DigestFocus) -> Result<JournalDigest, String> {
    let (events, journal_lines_non_empty, journal_lines_malformed) =
        session_journal::read_journal_for_digest(session_id).map_err(|e| e.to_string())?;
    let journal_file = session_journal::journal_file_path(session_id)
        .to_string_lossy()
        .into_owned();

    let mut turns_out: Vec<TurnRow> = Vec::new();
    let mut compaction_events = Vec::new();
    let mut stalls = Vec::new();
    let mut turn_errors = Vec::new();
    let mut other_errors = Vec::new();

    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_duration_ms: u64 = 0;
    let mut total_tool_calls: u64 = 0;
    let mut tool_calls_failed: u64 = 0;
    let mut turn_error_count = 0usize;
    let mut compact_count = 0usize;
    let mut stall_count = 0usize;
    let mut error_event_count = 0usize;
    let mut session_start_count = 0usize;
    let mut session_end_count = 0usize;

    let mut seq: u32 = 0;
    for ev in &events {
        match ev.event_type {
            JournalEventType::Turn => {
                seq += 1;
                let (ok_c, fail_c) = tool_call_counts(ev.tool_calls.as_ref());
                total_tool_calls += u64::from(ok_c + fail_c);
                tool_calls_failed += u64::from(fail_c);
                if let Some(ti) = ev.tokens_in {
                    total_tokens_in += ti;
                }
                if let Some(to) = ev.tokens_out {
                    total_tokens_out += to;
                }
                if let Some(d) = ev.duration_ms {
                    total_duration_ms += d;
                }

                let preview_len = match focus {
                    DigestFocus::All => 120,
                    DigestFocus::Summary => 0,
                };
                let user_input_preview = if preview_len == 0 {
                    String::new()
                } else {
                    preview(ev.user_input.as_ref(), preview_len)
                };

                let tools_selected_count = ev.tools_selected.as_ref().map_or(0, |v| v.len());
                let tools_used_count = ev.tools_used.as_ref().map_or(0, |v| v.len());
                let selected_skills = ev.selected_skills.clone().unwrap_or_default();

                let row = TurnRow {
                    seq,
                    turn_id: ev.turn,
                    ts: ev.ts.clone(),
                    model: ev.model.clone(),
                    tokens_in: ev.tokens_in,
                    tokens_out: ev.tokens_out,
                    duration_ms: ev.duration_ms,
                    ttft_ms: ev.ttft_ms,
                    context_ms: ev.context_ms,
                    selector_ms: ev.selector_ms,
                    selector_strategy: ev.selector_strategy.clone(),
                    selector_confidence: if matches!(focus, DigestFocus::All) {
                        ev.selector_confidence
                    } else {
                        None
                    },
                    routing_domain_hint: if matches!(focus, DigestFocus::All) {
                        ev.routing_domain_hint.clone()
                    } else {
                        None
                    },
                    entity_learn_skipped_no_domain: if matches!(focus, DigestFocus::All) {
                        Some(ev.entity_learn_skipped_no_domain)
                    } else {
                        None
                    },
                    memoria_ms: if matches!(focus, DigestFocus::All) {
                        ev.memoria_ms
                    } else {
                        None
                    },
                    tools_selected_count,
                    tools_used_count,
                    selected_skills: if matches!(focus, DigestFocus::All) {
                        selected_skills
                    } else {
                        Vec::new()
                    },
                    tool_calls_ok: ok_c,
                    tool_calls_fail: fail_c,
                    user_input_preview,
                    budget_pressure: ev.budget_pressure,
                };
                turns_out.push(row);
            }
            JournalEventType::TurnError => {
                turn_error_count += 1;
                turn_errors.push(TurnErrRow {
                    ts: ev.ts.clone(),
                    turn: ev.turn,
                    error: ev.error.clone().unwrap_or_default(),
                });
            }
            JournalEventType::Compact => {
                compact_count += 1;
                compaction_events.push(SideEvent {
                    ts: ev.ts.clone(),
                    kind: "compact".to_string(),
                    turn: ev.turn,
                    detail: json!({
                        "turns_compacted": ev.turns_compacted,
                        "facts_stored": ev.facts_stored,
                        "budget_pressure": ev.budget_pressure,
                    }),
                });
            }
            JournalEventType::StallDetected => {
                stall_count += 1;
                stalls.push(SideEvent {
                    ts: ev.ts.clone(),
                    kind: "stall".to_string(),
                    turn: ev.turn,
                    detail: json!({
                        "stall_type": ev.stall_type,
                        "error": ev.error,
                    }),
                });
            }
            JournalEventType::Error => {
                error_event_count += 1;
                other_errors.push(SideEvent {
                    ts: ev.ts.clone(),
                    kind: "error".to_string(),
                    turn: ev.turn,
                    detail: json!({ "message": ev.error }),
                });
            }
            JournalEventType::SessionStart => session_start_count += 1,
            JournalEventType::SessionEnd => session_end_count += 1,
            _ => {}
        }
    }

    let turn_count = turns_out.len();
    let (avg_tokens_in, avg_tokens_out, avg_duration_ms) = if turn_count == 0 {
        (0.0, 0.0, 0.0)
    } else {
        let n = turn_count as f64;
        (
            total_tokens_in as f64 / n,
            total_tokens_out as f64 / n,
            total_duration_ms as f64 / n,
        )
    };

    Ok(JournalDigest {
        schema_version: SCHEMA_VERSION,
        session_id: session_id.to_string(),
        journal_file,
        journal_lines_non_empty,
        journal_lines_malformed,
        aggregates: Aggregates {
            turn_count,
            turn_error_count,
            compact_count,
            stall_count,
            error_event_count,
            session_start_count,
            session_end_count,
            total_tokens_in,
            total_tokens_out,
            total_duration_ms,
            total_tool_calls,
            tool_calls_failed,
            avg_tokens_in,
            avg_tokens_out,
            avg_duration_ms,
        },
        turns: turns_out,
        compaction_events,
        stalls,
        turn_errors,
        other_errors,
    })
}

pub fn print_text(d: &JournalDigest) {
    use crossterm::style::Stylize;
    println!("  {} {}", "schema_version:".dim(), d.schema_version);
    println!("  {} {}", "session_id:".dim(), d.session_id.as_str().cyan());
    println!("  {} {}", "journal_file:".dim(), d.journal_file);
    println!(
        "  {} non_empty={} malformed={}",
        "journal_lines:".dim(),
        d.journal_lines_non_empty.to_string().cyan(),
        if d.journal_lines_malformed > 0 {
            d.journal_lines_malformed.to_string().red().to_string()
        } else {
            d.journal_lines_malformed.to_string()
        }
    );
    let a = &d.aggregates;
    println!(
        "\n  {}", "Aggregates".bold().cyan()
    );
    println!(
        "  turns={} turn_errors={} compacts={} stalls={} errors={}",
        a.turn_count.to_string().cyan(), a.turn_error_count, a.compact_count, a.stall_count, a.error_event_count
    );
    println!(
        "  tokens_in={} tokens_out={} duration_ms={} tool_calls={} tool_failures={}",
        a.total_tokens_in.to_string().cyan(), a.total_tokens_out.to_string().cyan(),
        a.total_duration_ms, a.total_tool_calls, a.tool_calls_failed
    );
    println!(
        "\n  {}", "Averages (per turn)".bold().cyan()
    );
    println!(
        "  tokens_in={:.1} tokens_out={:.1} duration_ms={:.1}",
        a.avg_tokens_in, a.avg_tokens_out, a.avg_duration_ms
    );
    if !d.turns.is_empty() {
        println!(
            "\n  {}", "Turns".bold().cyan()
        );
        println!(
            "  {}",
            format!("{:>4} {:>5} {:>7} {:>7} {:>8}  user_preview", "seq", "id", "tin", "tout", "ms").dim()
        );
        for t in &d.turns {
            let tin = t.tokens_in.unwrap_or(0);
            let tout = t.tokens_out.unwrap_or(0);
            let ms = t.duration_ms.unwrap_or(0);
            let tid = t
                .turn_id
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "  {:>4} {:>5} {:>7} {:>7} {:>8}  {}",
                t.seq, tid, tin, tout, ms, t.user_input_preview.as_str().dim()
            );
        }
    }
    if !d.compaction_events.is_empty() {
        println!("\n  {} {}", "compaction_events:".dim(), d.compaction_events.len().to_string().cyan());
        for e in &d.compaction_events {
            println!("    {} {} {}", e.ts.as_str().dim(), format!("turn={:?}", e.turn).dim(), e.detail);
        }
    }
    if !d.stalls.is_empty() {
        println!("\n  {} {}", "stalls:".yellow(), d.stalls.len().to_string().cyan());
        for e in &d.stalls {
            println!("    {} {} {}", e.ts.as_str().dim(), format!("turn={:?}", e.turn).dim(), e.detail);
        }
    }
    if !d.turn_errors.is_empty() {
        println!("\n  {} {}", "turn_errors:".red(), d.turn_errors.len().to_string().cyan());
        for e in &d.turn_errors {
            println!("    {} {} {}", e.ts.as_str().dim(), format!("turn={:?}", e.turn).dim(), e.error.as_str().red());
        }
    }
    if !d.other_errors.is_empty() {
        println!("\n  {} {}", "other_errors:".red(), d.other_errors.len().to_string().cyan());
        for e in &d.other_errors {
            println!("    {} {}", e.ts.as_str().dim(), e.detail);
        }
    }
}

pub fn run_digest(args: &super::JournalDigestArgs) -> Result<(), String> {
    let focus = parse_focus(args.focus.as_deref())?;
    let sid = resolve_session_for_digest(args.session_id.as_deref(), args.session.as_deref())?;
    let digest = build_digest(&sid, focus)?;
    let fmt = args.format.trim().to_ascii_lowercase();
    match fmt.as_str() {
        "json" => {
            let s = serde_json::to_string_pretty(&digest).map_err(|e| e.to_string())?;
            println!("{s}");
        }
        "text" => print_text(&digest),
        _ => {
            return Err(format!(
                "invalid --format '{}' (expected json or text)",
                args.format
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::JournalDirGuard;
    use std::fs;

    #[test]
    fn digest_counts_turns_and_aggregates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-digest-00000000-0000-0000-0000-000000000001";
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tokens_in":100,"tokens_out":20,"duration_ms":500,"user_input":"hi","tool_calls":[]}
{"type":"turn","ts":"2026-01-01T00:00:01Z","session_id":"S","turn":2,"tokens_in":200,"tokens_out":40,"duration_ms":600,"user_input":"bye","tool_calls":[{"name":"bash","ok":true,"ms":10}]}
{"type":"compact","ts":"2026-01-01T00:00:02Z","turns_compacted":1,"facts_stored":0}
"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.schema_version, SCHEMA_VERSION);
        assert_eq!(d.journal_lines_non_empty, 3);
        assert_eq!(d.journal_lines_malformed, 0);
        assert_eq!(d.turns.len(), 2);
        assert_eq!(d.aggregates.turn_count, 2);
        assert_eq!(d.aggregates.total_tokens_in, 300);
        assert_eq!(d.aggregates.total_tokens_out, 60);
        assert_eq!(d.aggregates.compact_count, 1);
        assert_eq!(d.turns[0].seq, 1);
        assert_eq!(d.turns[0].turn_id, Some(1));
        assert_eq!(d.turns[1].tool_calls_ok, 1);
    }

    #[test]
    fn digest_reports_malformed_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-malformed-00000000-0000-0000-0000-000000000002";
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            "{\"type\":\"turn\",\"ts\":\"2026-01-01T00:00:00Z\",\"turn\":1,\"tool_calls\":[]}\nnot json\n",
        )
        .expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.journal_lines_non_empty, 2);
        assert_eq!(d.journal_lines_malformed, 1);
        assert_eq!(d.aggregates.turn_count, 1);
    }

    #[test]
    fn digest_errors_when_journal_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let err = build_digest(
            "missing-session-00000000-0000-0000-0000-000000000099",
            DigestFocus::All,
        )
        .err()
        .expect("expected missing file");
        assert!(
            err.contains("not found") || err.contains("journal"),
            "{err}"
        );
    }
}

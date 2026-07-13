//! `astra journal trace <session>` — export the EXPLAIN ANALYZE pressure/tier
//! trace table for a bridge session.
//!
//! The bridge journals a `PipelineMetrics` event before each LLM call
//! (pressure, tier, reserve) and `PipelineFeedback`/`PipelineAlert` events
//! after the response (cache-hit ratio, fired alert rules). This command
//! reads a session's journal in emission order, pairs each metrics event with
//! the feedback/alerts that follow it — up to the next metrics event — into
//! one row per call, and renders the result with the same
//! `render_markdown`/`render_csv` used for paper trace tables.
//!
//! Positional pairing, not `turn`-field matching: `PipelineMetrics.turn` is a
//! live per-call round counter, but `PipelineFeedback`/`PipelineAlert` events
//! carry the coarser session-level turn number (often constant across every
//! call in a session), so grouping by `.turn` would silently merge every row
//! in a session into one. Emission order is the only reliable join key.

use astra_services::session_journal::{self, JournalEvent, JournalEventType};
use astra_turn_core::compaction_types::CompactionTier;
use astra_turn_core::pipeline_journal::{PipelineEventKind, PipelineJournalEvent};
use astra_turn_core::pipeline_trace_export::{TraceRow, render_csv, render_markdown};

use crate::cli::cli_config::cli_args;
use crate::cli::journal_digest;

fn parse_tier(raw: Option<&str>) -> CompactionTier {
    match raw {
        Some("TrimSchemas") => CompactionTier::TrimSchemas,
        Some("CompactHistory") => CompactionTier::CompactHistory,
        Some("AggressivePrune") => CompactionTier::AggressivePrune,
        _ => CompactionTier::Normal,
    }
}

/// Build one `TraceRow` per LLM call from a session's journaled pipeline
/// events, in emission order.
fn rows_from_journal(events: &[JournalEvent]) -> Vec<TraceRow> {
    let mut rows = Vec::new();
    let mut pending: Option<TraceRow> = None;

    for evt in events {
        if !matches!(
            evt.event_type,
            JournalEventType::PipelineMetrics
                | JournalEventType::PipelineFeedback
                | JournalEventType::PipelineAlert
        ) {
            continue;
        }
        let Some(payload) = evt.metadata.as_ref() else {
            continue;
        };
        let Ok(pipeline_evt) = serde_json::from_value::<PipelineJournalEvent>(payload.clone())
        else {
            continue;
        };

        match pipeline_evt.kind {
            PipelineEventKind::Metrics => {
                if let Some(row) = pending.take() {
                    rows.push(row);
                }
                pending = Some(TraceRow {
                    turn: pipeline_evt.turn.saturating_add(1),
                    raw_pressure: pipeline_evt.raw_pressure.unwrap_or(0.0),
                    predictive_pressure: pipeline_evt.predictive_pressure.unwrap_or(0.0),
                    tier: parse_tier(pipeline_evt.tier.as_deref()),
                    cache_hit_ratio: None,
                    event: String::new(),
                    spilled: pipeline_evt.spilled.unwrap_or(0),
                });
            }
            PipelineEventKind::Feedback => {
                if let Some(row) = pending.as_mut() {
                    row.cache_hit_ratio = pipeline_evt.cache_hit_ratio;
                }
            }
            PipelineEventKind::Alert => {
                if let (Some(row), Some(rule)) = (pending.as_mut(), pipeline_evt.alert_rule) {
                    if row.event.is_empty() {
                        row.event = rule;
                    } else {
                        row.event.push_str(", ");
                        row.event.push_str(&rule);
                    }
                }
            }
            PipelineEventKind::CompactionAudit => {}
        }
    }
    if let Some(row) = pending.take() {
        rows.push(row);
    }
    rows
}

pub(crate) fn run_trace(args: &cli_args::JournalTraceArgs) -> Result<(), String> {
    let session_id = journal_digest::resolve_session_for_digest(
        args.session_id.as_deref(),
        args.session.as_deref(),
    )?;
    let events = session_journal::read_journal(&session_id).map_err(|e| e.to_string())?;
    let rows = rows_from_journal(&events);
    if rows.is_empty() {
        eprintln!(
            "[journal trace] no PipelineMetrics events found for session {session_id} \
             (nothing to export — this session may predate the bridge's trace journaling, \
             or ran with no pipeline calls)"
        );
    }

    let format = args.format.trim().to_ascii_lowercase();
    let rendered = match format.as_str() {
        "" | "markdown" | "md" => render_markdown(&rows),
        "csv" => render_csv(&rows),
        other => {
            return Err(format!(
                "invalid --format '{other}' (expected markdown or csv)"
            ));
        }
    };

    match args.out.as_deref() {
        Some(path) => {
            std::fs::write(path, &rendered)
                .map_err(|e| format!("failed to write trace to '{path}': {e}"))?;
            eprintln!(
                "[journal trace] wrote {} row(s) for session {session_id} to {path}",
                rows.len()
            );
            Ok(())
        }
        None => {
            print!("{rendered}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn evt(event_type: JournalEventType, turn: u32, metadata: serde_json::Value) -> JournalEvent {
        let mut e = JournalEvent::pipeline_metrics(Some("s"), turn, metadata);
        e.event_type = event_type;
        e
    }

    fn metrics_payload(turn: u32, raw: f64, tier: &str) -> serde_json::Value {
        json!({
            "kind": "Metrics",
            "turn": turn,
            "raw_pressure": raw,
            "predictive_pressure": raw + 0.02,
            "tier": tier,
            "spilled": 0,
        })
    }

    fn feedback_payload(hit_ratio: f64) -> serde_json::Value {
        json!({"kind": "Feedback", "turn": 1, "cache_hit_ratio": hit_ratio})
    }

    fn alert_payload(rule: &str) -> serde_json::Value {
        json!({"kind": "Alert", "turn": 1, "alert_rule": rule})
    }

    #[test]
    fn positional_pairing_survives_coarse_shared_turn_numbers() {
        // Every feedback/alert event below shares turn=1 (the realistic
        // bridge shape), while metrics events carry the real per-call
        // counter (0, 1, 2). Grouping by `.turn` would merge all three
        // calls into one bucket; positional pairing must not.
        let events = vec![
            evt(
                JournalEventType::PipelineMetrics,
                0,
                metrics_payload(0, 0.10, "Normal"),
            ),
            evt(
                JournalEventType::PipelineFeedback,
                1,
                feedback_payload(0.20),
            ),
            evt(
                JournalEventType::PipelineMetrics,
                1,
                metrics_payload(1, 0.55, "TrimSchemas"),
            ),
            evt(
                JournalEventType::PipelineFeedback,
                1,
                feedback_payload(0.90),
            ),
            evt(
                JournalEventType::PipelineAlert,
                1,
                alert_payload("pressure_spike"),
            ),
            evt(
                JournalEventType::PipelineMetrics,
                2,
                metrics_payload(2, 0.60, "TrimSchemas"),
            ),
            evt(
                JournalEventType::PipelineFeedback,
                1,
                feedback_payload(0.95),
            ),
        ];
        let rows = rows_from_journal(&events);
        assert_eq!(rows.len(), 3, "one row per metrics event");
        assert_eq!(rows[0].turn, 1);
        assert!((rows[0].raw_pressure - 0.10).abs() < 1e-9);
        assert!((rows[0].cache_hit_ratio.unwrap() - 0.20).abs() < 1e-9);
        assert_eq!(rows[0].event, "");

        assert_eq!(rows[1].turn, 2);
        assert_eq!(rows[1].tier, CompactionTier::TrimSchemas);
        assert!((rows[1].cache_hit_ratio.unwrap() - 0.90).abs() < 1e-9);
        assert_eq!(rows[1].event, "pressure_spike");

        assert_eq!(rows[2].turn, 3);
        assert!((rows[2].cache_hit_ratio.unwrap() - 0.95).abs() < 1e-9);
    }

    #[test]
    fn multiple_alerts_on_one_call_join_with_comma() {
        let events = vec![
            evt(
                JournalEventType::PipelineMetrics,
                5,
                metrics_payload(5, 0.62, "TrimSchemas"),
            ),
            evt(
                JournalEventType::PipelineAlert,
                1,
                alert_payload("pressure_spike"),
            ),
            evt(
                JournalEventType::PipelineAlert,
                1,
                alert_payload("emergent_overflow"),
            ),
        ];
        let rows = rows_from_journal(&events);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event, "pressure_spike, emergent_overflow");
    }

    #[test]
    fn feedback_before_any_metrics_is_dropped_not_panicked() {
        let events = vec![evt(
            JournalEventType::PipelineFeedback,
            1,
            feedback_payload(0.5),
        )];
        assert_eq!(rows_from_journal(&events).len(), 0);
    }

    #[test]
    fn non_pipeline_events_are_ignored() {
        let mut turn_evt = JournalEvent::pipeline_metrics(Some("s"), 0, json!({}));
        turn_evt.event_type = JournalEventType::Turn;
        let events = vec![
            turn_evt,
            evt(
                JournalEventType::PipelineMetrics,
                0,
                metrics_payload(0, 0.3, "Normal"),
            ),
        ];
        assert_eq!(rows_from_journal(&events).len(), 1);
    }

    #[test]
    fn unknown_tier_string_falls_back_to_normal() {
        assert_eq!(parse_tier(Some("SomethingNew")), CompactionTier::Normal);
        assert_eq!(parse_tier(None), CompactionTier::Normal);
        assert_eq!(
            parse_tier(Some("AggressivePrune")),
            CompactionTier::AggressivePrune
        );
    }

    #[test]
    fn markdown_and_csv_formats_both_render() {
        let rows = rows_from_journal(&[evt(
            JournalEventType::PipelineMetrics,
            0,
            metrics_payload(0, 0.42, "Normal"),
        )]);
        assert!(render_markdown(&rows).contains("| 1 |"));
        assert!(render_csv(&rows).starts_with("turn,raw_pressure"));
    }
}

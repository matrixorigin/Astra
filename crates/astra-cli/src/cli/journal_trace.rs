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
use astra_turn_core::pipeline_journal::{PipelineEventKind, PipelineJournalEvent};
use astra_turn_core::pipeline_trace_export::{TraceRow, render_csv, render_markdown};

use crate::cli::cli_config::cli_args;
use crate::cli::journal_digest;

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
                    // Typed end-to-end: an unknown tier variant fails the
                    // event's deserialization above (the row is skipped
                    // loudly), never silently mislabeled — only a metrics
                    // event with the field absent falls back to Normal.
                    tier: pipeline_evt.tier.unwrap_or_default(),
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
    use astra_turn_core::compaction_types::CompactionTier;
    use serde_json::json;

    fn evt(event_type: JournalEventType, turn: u32, metadata: serde_json::Value) -> JournalEvent {
        let mut e = JournalEvent::pipeline_metrics(Some("s"), turn, metadata);
        e.event_type = event_type;
        e
    }

    /// `tier` is the serde (snake_case) form, matching what
    /// `PipelineJournalEvent::from_metrics` actually serializes.
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
                metrics_payload(0, 0.10, "normal"),
            ),
            evt(
                JournalEventType::PipelineFeedback,
                1,
                feedback_payload(0.20),
            ),
            evt(
                JournalEventType::PipelineMetrics,
                1,
                metrics_payload(1, 0.55, "trim_schemas"),
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
                metrics_payload(2, 0.60, "trim_schemas"),
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
                metrics_payload(5, 0.62, "trim_schemas"),
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
                metrics_payload(0, 0.3, "normal"),
            ),
        ];
        assert_eq!(rows_from_journal(&events).len(), 1);
    }

    #[test]
    fn unknown_tier_variant_skips_event_loudly_instead_of_masking_as_normal() {
        // Version skew (a tier variant this binary doesn't know) must drop
        // the row — visible as a shorter table — not silently relabel it
        // Normal in a paper-facing evidence table.
        let events = vec![
            evt(
                JournalEventType::PipelineMetrics,
                0,
                metrics_payload(0, 0.62, "some_future_tier"),
            ),
            evt(
                JournalEventType::PipelineMetrics,
                1,
                metrics_payload(1, 0.30, "trim_schemas"),
            ),
        ];
        let rows = rows_from_journal(&events);
        assert_eq!(rows.len(), 1, "unknown-variant event must be skipped");
        assert_eq!(rows[0].tier, CompactionTier::TrimSchemas);
    }

    #[test]
    fn round_trip_from_metrics_serialization_parses_back() {
        // Pin the writer↔reader seam end-to-end: serialize with the REAL
        // producer (from_metrics) and assemble from that exact payload.
        use astra_turn_core::context_pipeline::PipelineRunMetrics;
        let metrics = PipelineRunMetrics {
            turn_index: 4,
            input_tokens: 40_000,
            output_reserve_tokens: 1_200,
            raw_pressure: 0.61,
            predictive_pressure: 0.66,
            compact_tier: CompactionTier::TrimSchemas,
            sections: 5,
            messages: 40,
            tool_schemas: 9,
            cache_markers: 2,
            tokens_cleared: 0,
            avg_cache_hit_ratio: 0.9,
            spilled: 3,
            api_calls_total: 5,
        };
        let payload = serde_json::to_value(
            astra_turn_core::pipeline_journal::PipelineJournalEvent::from_metrics(
                &metrics, "memory",
            ),
        )
        .expect("serialize");
        let rows = rows_from_journal(&[evt(JournalEventType::PipelineMetrics, 4, payload)]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tier, CompactionTier::TrimSchemas);
        assert_eq!(rows[0].turn, 5);
        assert_eq!(rows[0].spilled, 3);
        assert!((rows[0].raw_pressure - 0.61).abs() < 1e-9);
    }

    #[test]
    fn markdown_and_csv_formats_both_render() {
        let rows = rows_from_journal(&[evt(
            JournalEventType::PipelineMetrics,
            0,
            metrics_payload(0, 0.42, "normal"),
        )]);
        assert!(render_markdown(&rows).contains("| 1 |"));
        assert!(render_csv(&rows).starts_with("turn,raw_pressure"));
    }
}

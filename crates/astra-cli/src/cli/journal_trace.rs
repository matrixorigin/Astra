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
//! Join strategy: events stamped with the shared per-call `round` key join
//! exactly, regardless of emission order — the key exists precisely because
//! `turn` lives in two spaces (Metrics carry the per-call round counter;
//! Feedback/Alert carry the coarser session-level turn, often constant for a
//! whole session). Events without the key (journals written before it
//! existed) fall back to positional pairing: attach to the most recent
//! metrics row. Positional pairing alone mis-attributed hit ratios when the
//! two event kinds arrived in different orders (last row's cache-hit showed
//! `-` while its feedback value had been absorbed by the previous row).

use astra_services::session_journal::{self, JournalEvent, JournalEventType};
use astra_turn_core::pipeline_journal::{PipelineEventKind, PipelineJournalEvent};
use astra_turn_core::pipeline_trace_export::{TraceRow, render_csv, render_markdown};

use crate::cli::cli_config::cli_args;
use crate::cli::journal_digest;

/// Feedback/alert data that arrived before its own metrics event
/// (emission-order inversion); applied when the matching row opens.
#[derive(Default)]
struct HeldRoundData {
    cache_hit_ratio: Option<f64>,
    alert_rules: Vec<String>,
}

fn append_alert_rule(event: &mut String, rule: &str) {
    if event.is_empty() {
        event.push_str(rule);
    } else {
        event.push_str(", ");
        event.push_str(rule);
    }
}

/// Build one `TraceRow` per LLM call from a session's journaled pipeline
/// events: exact join on the shared `round` key when present, positional
/// (most-recent-metrics-row) fallback for legacy events without it.
fn rows_from_journal(events: &[JournalEvent]) -> Vec<TraceRow> {
    let mut rows: Vec<TraceRow> = Vec::new();
    let mut index_by_round: std::collections::HashMap<u32, usize> =
        std::collections::HashMap::new();
    let mut held: std::collections::HashMap<u32, HeldRoundData> = std::collections::HashMap::new();

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
                // Metrics events always carry the per-call counter in `turn`;
                // `round` (when stamped) duplicates it for uniformity.
                let round = pipeline_evt.round.unwrap_or(pipeline_evt.turn);
                let mut row = TraceRow {
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
                };
                if let Some(early) = held.remove(&round) {
                    row.cache_hit_ratio = early.cache_hit_ratio;
                    for rule in &early.alert_rules {
                        append_alert_rule(&mut row.event, rule);
                    }
                }
                index_by_round.insert(round, rows.len());
                rows.push(row);
            }
            PipelineEventKind::Feedback => match pipeline_evt.round {
                Some(round) => match index_by_round.get(&round) {
                    Some(&idx) => rows[idx].cache_hit_ratio = pipeline_evt.cache_hit_ratio,
                    None => {
                        held.entry(round).or_default().cache_hit_ratio =
                            pipeline_evt.cache_hit_ratio;
                    }
                },
                None => {
                    if let Some(row) = rows.last_mut() {
                        row.cache_hit_ratio = pipeline_evt.cache_hit_ratio;
                    }
                }
            },
            PipelineEventKind::Alert => {
                let Some(rule) = pipeline_evt.alert_rule else {
                    continue;
                };
                match pipeline_evt.round {
                    Some(round) => match index_by_round.get(&round) {
                        Some(&idx) => append_alert_rule(&mut rows[idx].event, &rule),
                        None => held.entry(round).or_default().alert_rules.push(rule),
                    },
                    None => {
                        if let Some(row) = rows.last_mut() {
                            append_alert_rule(&mut row.event, &rule);
                        }
                    }
                }
            }
            PipelineEventKind::CompactionAudit => {}
        }
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

    fn keyed(mut payload: serde_json::Value, round: u32) -> serde_json::Value {
        payload["round"] = json!(round);
        payload
    }

    #[test]
    fn keyed_join_survives_feedback_before_metrics_order() {
        // The systematic inversion: every feedback event lands BEFORE its own
        // metrics event. Positional pairing shifted every hit value by one
        // round and dropped the first; the round key must pair exactly.
        let events = vec![
            evt(
                JournalEventType::PipelineFeedback,
                1,
                keyed(feedback_payload(0.20), 0),
            ),
            evt(
                JournalEventType::PipelineMetrics,
                0,
                keyed(metrics_payload(0, 0.10, "normal"), 0),
            ),
            evt(
                JournalEventType::PipelineFeedback,
                1,
                keyed(feedback_payload(0.90), 1),
            ),
            evt(
                JournalEventType::PipelineMetrics,
                1,
                keyed(metrics_payload(1, 0.30, "normal"), 1),
            ),
        ];
        let rows = rows_from_journal(&events);
        assert_eq!(rows.len(), 2);
        assert!((rows[0].cache_hit_ratio.unwrap() - 0.20).abs() < 1e-9);
        assert!((rows[1].cache_hit_ratio.unwrap() - 0.90).abs() < 1e-9);
    }

    #[test]
    fn keyed_join_fixes_last_row_pair_inversion() {
        // The reported symptom: 26 metrics + 26 feedback events, but the
        // final pair arrives inverted — positionally the last feedback
        // overwrote the previous row and the last row rendered `-`.
        let events = vec![
            evt(
                JournalEventType::PipelineMetrics,
                0,
                keyed(metrics_payload(0, 0.10, "normal"), 0),
            ),
            evt(
                JournalEventType::PipelineFeedback,
                1,
                keyed(feedback_payload(0.98), 0),
            ),
            evt(
                JournalEventType::PipelineFeedback,
                1,
                keyed(feedback_payload(0.66), 1),
            ),
            evt(
                JournalEventType::PipelineMetrics,
                1,
                keyed(metrics_payload(1, 0.30, "normal"), 1),
            ),
        ];
        let rows = rows_from_journal(&events);
        assert_eq!(rows.len(), 2);
        assert!((rows[0].cache_hit_ratio.unwrap() - 0.98).abs() < 1e-9);
        assert!(
            (rows[1].cache_hit_ratio.unwrap() - 0.66).abs() < 1e-9,
            "last row must get its own feedback, not `-`"
        );
    }

    #[test]
    fn keyed_late_alert_attaches_to_its_round_after_next_metrics_opened() {
        let events = vec![
            evt(
                JournalEventType::PipelineMetrics,
                0,
                keyed(metrics_payload(0, 0.55, "trim_schemas"), 0),
            ),
            evt(
                JournalEventType::PipelineMetrics,
                1,
                keyed(metrics_payload(1, 0.60, "trim_schemas"), 1),
            ),
            evt(
                JournalEventType::PipelineAlert,
                1,
                keyed(alert_payload("pressure_spike"), 0),
            ),
        ];
        let rows = rows_from_journal(&events);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].event, "pressure_spike");
        assert_eq!(rows[1].event, "");
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

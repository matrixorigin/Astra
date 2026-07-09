//! Session trace exporter — renders per-turn pipeline records into the
//! compact trace-table format used in EXPLAIN ANALYZE reports and papers:
//! one row per turn with pressure, tier, cache behavior, event, and spills.
//!
//! The exporter is presentation-only: rows are built from records the
//! pipeline already emits (`PipelineRunMetrics` per turn, `ContextFeedback`
//! after the API response, `TraceAlert`s from rule evaluation). No new
//! instrumentation is required to produce a table.

use serde::{Deserialize, Serialize};

use crate::compaction_types::CompactionTier;
use crate::context_feedback::ContextFeedback;
use crate::context_pipeline::PipelineRunMetrics;
use crate::trace_alert::TraceAlert;

/// One row of the exported trace table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRow {
    pub turn: u32,
    pub raw_pressure: f64,
    pub predictive_pressure: f64,
    pub tier: CompactionTier,
    /// Cache-hit ratio from post-execution feedback; `None` before feedback.
    pub cache_hit_ratio: Option<f64>,
    /// Event label for the turn (alert rules fired, recovery escalations…).
    /// Empty when the turn was uneventful.
    pub event: String,
    /// Sections spilled to the spill backend this turn.
    pub spilled: u32,
}

impl TraceRow {
    /// Build a row from a turn's pre-execution metrics, optional
    /// post-execution feedback, and the alerts that fired on the turn.
    #[must_use]
    pub fn from_turn(
        metrics: &PipelineRunMetrics,
        feedback: Option<&ContextFeedback>,
        alerts: &[TraceAlert],
    ) -> Self {
        let event = alerts
            .iter()
            .map(|a| a.rule.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            turn: metrics.turn_index,
            raw_pressure: metrics.raw_pressure,
            predictive_pressure: metrics.predictive_pressure,
            tier: metrics.compact_tier,
            cache_hit_ratio: feedback.map(|f| f.cache_hit_ratio),
            event,
            spilled: metrics.spilled,
        }
    }
}

/// Render rows as a GitHub-flavored markdown table.
#[must_use]
pub fn render_markdown(rows: &[TraceRow]) -> String {
    let mut out = String::from(
        "| turn | P_raw | P_pred | tier | cache hit | event | spilled |\n\
         |---:|---:|---:|:--|---:|:--|---:|\n",
    );
    for row in rows {
        out.push_str(&format!(
            "| {} | {:.2} | {:.2} | {} | {} | {} | {} |\n",
            row.turn,
            row.raw_pressure,
            row.predictive_pressure,
            tier_label(row.tier),
            hit_label(row.cache_hit_ratio),
            if row.event.is_empty() {
                "-"
            } else {
                &row.event
            },
            row.spilled,
        ));
    }
    out
}

/// Render rows as CSV (header + one line per turn).
#[must_use]
pub fn render_csv(rows: &[TraceRow]) -> String {
    let mut out =
        String::from("turn,raw_pressure,predictive_pressure,tier,cache_hit,event,spilled\n");
    for row in rows {
        out.push_str(&format!(
            "{},{:.4},{:.4},{},{},{},{}\n",
            row.turn,
            row.raw_pressure,
            row.predictive_pressure,
            tier_label(row.tier),
            row.cache_hit_ratio
                .map(|h| format!("{h:.4}"))
                .unwrap_or_default(),
            csv_escape(&row.event),
            row.spilled,
        ));
    }
    out
}

fn tier_label(tier: CompactionTier) -> &'static str {
    match tier {
        CompactionTier::Normal => "Normal",
        CompactionTier::TrimSchemas => "TrimSchemas",
        CompactionTier::CompactHistory => "CompactHistory",
        CompactionTier::AggressivePrune => "AggressivePrune",
    }
}

fn hit_label(hit: Option<f64>) -> String {
    match hit {
        Some(h) => format!("{:.0}%", h * 100.0),
        None => "-".to_string(),
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_alert::AlertSeverity;

    fn row(turn: u32, tier: CompactionTier, hit: Option<f64>, event: &str) -> TraceRow {
        TraceRow {
            turn,
            raw_pressure: 0.42,
            predictive_pressure: 0.61,
            tier,
            cache_hit_ratio: hit,
            event: event.to_string(),
            spilled: 1,
        }
    }

    #[test]
    fn markdown_renders_one_line_per_turn_with_header() {
        let rows = vec![
            row(1, CompactionTier::Normal, None, ""),
            row(2, CompactionTier::TrimSchemas, Some(0.87), "pressure_spike"),
        ];
        let md = render_markdown(&rows);
        let lines: Vec<&str> = md.lines().collect();
        assert_eq!(lines.len(), 4, "header + separator + 2 rows");
        assert!(lines[2].contains("| 1 |"));
        assert!(lines[2].contains("Normal"));
        assert!(lines[2].contains("| - |"), "no feedback → hit placeholder");
        assert!(lines[3].contains("87%"));
        assert!(lines[3].contains("pressure_spike"));
    }

    #[test]
    fn csv_escapes_multi_event_rows() {
        let rows = vec![row(
            3,
            CompactionTier::CompactHistory,
            Some(0.5),
            "recovery_loop, compaction_cascade",
        )];
        let csv = render_csv(&rows);
        assert!(csv.contains("\"recovery_loop, compaction_cascade\""));
        assert!(csv.starts_with("turn,raw_pressure"));
    }

    #[test]
    fn from_turn_joins_alert_rules_into_event() {
        let metrics = PipelineRunMetrics {
            turn_index: 7,
            input_tokens: 1000,
            output_reserve_tokens: 500,
            raw_pressure: 0.55,
            predictive_pressure: 0.72,
            compact_tier: CompactionTier::TrimSchemas,
            sections: 5,
            messages: 3,
            tool_schemas: 2,
            cache_markers: 2,
            tokens_cleared: 0,
            avg_cache_hit_ratio: 0.8,
            spilled: 0,
            api_calls_total: 6,
        };
        let feedback = ContextFeedback::from_usage(1000, 800, 200, 100, false);
        let alerts = vec![
            TraceAlert {
                severity: AlertSeverity::Warning,
                rule: "pressure_spike".into(),
                message: String::new(),
                turn: 7,
            },
            TraceAlert {
                severity: AlertSeverity::Warning,
                rule: "emergent_overflow".into(),
                message: String::new(),
                turn: 7,
            },
        ];
        let row = TraceRow::from_turn(&metrics, Some(&feedback), &alerts);
        assert_eq!(row.turn, 7);
        assert_eq!(row.event, "pressure_spike, emergent_overflow");
        assert!((row.cache_hit_ratio.unwrap() - 0.8).abs() < 1e-9);
    }
}

//! Session Replay Harness
//!
//! Loads JSONL session journal files and extracts reliability metrics so that
//! known-bad sessions can serve as regression fixtures.  When the runtime
//! changes, the harness re-evaluates the same session to ensure:
//!
//! 1. **Checkpoint salvage** — interrupted turns must have a checkpoint event.
//! 2. **Approval reduction** — approval prompts should not exceed a per-turn cap.
//! 3. **Selector confidence health** — confidence should not collapse to the
//!    floor for consecutive turns.
//! 4. **Interruption recording** — fatal errors should produce a structured
//!    `InterruptionRecorded` event with resume guidance.

// Re-export from services crate for convenience.
pub use astra_services::session_journal::{JournalEvent, JournalEventType};

// ─── Replay Metrics ──────────────────────────────────────────────────────

/// Aggregated reliability metrics extracted from a session journal.
#[derive(Debug, Default)]
pub struct SessionReplayMetrics {
    /// Total number of events in the journal.
    pub total_events: usize,
    /// Number of Turn events.
    pub turn_count: usize,
    /// Number of TurnError events.
    pub turn_error_count: usize,
    /// Number of Checkpoint events.
    pub checkpoint_count: usize,
    /// Number of ApprovalRequired events.
    pub approval_required_count: usize,
    /// Number of ApprovalDecision events.
    pub approval_decision_count: usize,
    /// Number of InterruptionRecorded events.
    pub interruption_count: usize,
    /// Number of Compact events.
    pub compact_count: usize,
    /// Selector confidence values per turn (turn_number, confidence).
    pub confidence_values: Vec<(u32, f64)>,
    /// Per-turn approval counts (turn_number, count).
    pub per_turn_approvals: Vec<(u32, usize)>,
    /// Turns that had errors but no preceding checkpoint.
    pub unsalvaged_error_turns: Vec<u32>,
    /// Turns with tool failure rate > threshold (turn_number, failure_rate).
    pub high_failure_turns: Vec<(u32, f64)>,
    /// Consecutive turns where confidence was at or below the floor.
    pub max_consecutive_low_confidence: usize,
    /// Number of context-window error events (413 / prompt-too-long).
    pub context_window_error_count: usize,
    /// Compaction events that were followed by another context-window error
    /// on the same turn (compaction was insufficient).
    pub ineffective_compaction_count: usize,
    /// Total tokens_in across all turns (for cost tracking).
    pub total_tokens_in: u64,
    /// Total tokens_out across all turns.
    pub total_tokens_out: u64,
    /// Number of turns that had stall_detected events.
    pub stall_count: usize,
}

impl SessionReplayMetrics {
    /// Whether the session has any unsalvaged error turns (errors without checkpoints).
    pub fn has_unsalvaged_errors(&self) -> bool {
        !self.unsalvaged_error_turns.is_empty()
    }

    /// Maximum number of approval prompts in any single turn.
    pub fn max_approvals_per_turn(&self) -> usize {
        self.per_turn_approvals
            .iter()
            .map(|(_, count)| *count)
            .max()
            .unwrap_or(0)
    }

    /// Average selector confidence (NaN if no data).
    pub fn avg_confidence(&self) -> f64 {
        if self.confidence_values.is_empty() {
            return f64::NAN;
        }
        let sum: f64 = self.confidence_values.iter().map(|(_, c)| c).sum();
        sum / self.confidence_values.len() as f64
    }
}

// ─── Parsing ──────────────────────────────────────────────────────────────

/// Parse a JSONL string into journal events, skipping malformed lines.
pub fn parse_journal_lines(content: &str) -> Vec<JournalEvent> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<JournalEvent>(line).ok())
        .collect()
}

/// Extract reliability metrics from a list of journal events.
pub fn extract_metrics(events: &[JournalEvent]) -> SessionReplayMetrics {
    let mut m = SessionReplayMetrics {
        total_events: events.len(),
        ..Default::default()
    };

    let mut checkpoint_turns: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut error_turns: Vec<u32> = Vec::new();
    let mut approval_by_turn: std::collections::HashMap<u32, usize> =
        std::collections::HashMap::new();
    // Track compaction and context-window events per turn for effectiveness analysis.
    let mut compact_turns: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut context_error_turns: std::collections::HashSet<u32> = std::collections::HashSet::new();

    for event in events {
        match event.event_type {
            JournalEventType::Turn => {
                m.turn_count += 1;
                if let Some(conf) = event.selector_confidence {
                    if let Some(turn) = event.turn {
                        m.confidence_values.push((turn, conf));
                    }
                }
                // Accumulate token usage.
                if let Some(ti) = event.tokens_in {
                    m.total_tokens_in += ti;
                }
                if let Some(to) = event.tokens_out {
                    m.total_tokens_out += to;
                }
                // Check tool failure rate from tool_calls.
                if let Some(ref calls) = event.tool_calls {
                    let total = calls.len();
                    let failed = calls.iter().filter(|tc| !tc.ok).count();
                    if total > 0 {
                        let rate = failed as f64 / total as f64;
                        if rate > 0.3 && total >= 3 {
                            if let Some(turn) = event.turn {
                                m.high_failure_turns.push((turn, rate));
                            }
                        }
                    }
                }
            }
            JournalEventType::TurnError => {
                m.turn_error_count += 1;
                if let Some(turn) = event.turn {
                    error_turns.push(turn);
                }
                // Detect context-window errors from error message.
                if let Some(ref err) = event.error {
                    let lower = err.to_lowercase();
                    if lower.contains("context_window")
                        || lower.contains("context window")
                        || lower.contains("prompt is too long")
                        || lower.contains("too many tokens")
                    {
                        m.context_window_error_count += 1;
                        if let Some(turn) = event.turn {
                            context_error_turns.insert(turn);
                        }
                    }
                }
            }
            JournalEventType::Checkpoint => {
                m.checkpoint_count += 1;
                if let Some(turn) = event.turn {
                    checkpoint_turns.insert(turn);
                }
            }
            JournalEventType::ApprovalRequired => {
                m.approval_required_count += 1;
                if let Some(turn) = event.turn {
                    *approval_by_turn.entry(turn).or_insert(0) += 1;
                }
            }
            JournalEventType::ApprovalDecision => {
                m.approval_decision_count += 1;
            }
            JournalEventType::InterruptionRecorded => {
                m.interruption_count += 1;
            }
            JournalEventType::Compact => {
                m.compact_count += 1;
                if let Some(turn) = event.turn {
                    compact_turns.insert(turn);
                }
            }
            JournalEventType::StallDetected => {
                m.stall_count += 1;
            }
            _ => {}
        }
    }

    // Unsalvaged errors: error turns that don't have a checkpoint on the same or previous turn.
    for &t in &error_turns {
        if !checkpoint_turns.contains(&t) && (t == 0 || !checkpoint_turns.contains(&(t - 1))) {
            m.unsalvaged_error_turns.push(t);
        }
    }

    // Ineffective compaction: turns that had compaction AND a subsequent context-window error.
    for &t in &compact_turns {
        if context_error_turns.contains(&t) {
            m.ineffective_compaction_count += 1;
        }
    }

    // Per-turn approvals.
    m.per_turn_approvals = approval_by_turn.into_iter().collect();
    m.per_turn_approvals.sort_by_key(|(t, _)| *t);

    // Consecutive low confidence.
    const CONFIDENCE_FLOOR: f64 = 0.35;
    let mut consecutive = 0usize;
    let mut max_consecutive = 0usize;
    for &(_, conf) in &m.confidence_values {
        if conf <= CONFIDENCE_FLOOR {
            consecutive += 1;
            max_consecutive = max_consecutive.max(consecutive);
        } else {
            consecutive = 0;
        }
    }
    m.max_consecutive_low_confidence = max_consecutive;

    m
}

// ─── Invariant Checks ─────────────────────────────────────────────────────

/// Configuration for replay invariant checks.
#[derive(Debug, Clone)]
pub struct ReplayInvariantConfig {
    /// Maximum approval prompts allowed per turn.
    pub max_approvals_per_turn: usize,
    /// Maximum consecutive turns at or below the confidence floor.
    pub max_consecutive_low_confidence: usize,
    /// Tool failure rate threshold (0.0–1.0) for flagging turns.
    pub tool_failure_rate_threshold: f64,
    /// Maximum allowed stall events before flagging.
    pub max_stall_count: usize,
    /// Maximum allowed ineffective compaction events (compacted but still hit 413).
    pub max_ineffective_compactions: usize,
}

impl Default for ReplayInvariantConfig {
    fn default() -> Self {
        Self {
            max_approvals_per_turn: 15,
            max_consecutive_low_confidence: 4,
            tool_failure_rate_threshold: 0.3,
            max_stall_count: 3,
            max_ineffective_compactions: 2,
        }
    }
}

/// A single invariant violation found during replay analysis.
#[derive(Debug)]
pub struct InvariantViolation {
    /// Which invariant was violated.
    pub invariant: &'static str,
    /// Severity: "error" for must-fix, "warning" for degradation.
    pub severity: &'static str,
    /// Human-readable description.
    pub description: String,
}

/// Check replay metrics against invariants and return violations.
pub fn check_invariants(
    metrics: &SessionReplayMetrics,
    config: &ReplayInvariantConfig,
) -> Vec<InvariantViolation> {
    let mut violations = Vec::new();

    // 1. Checkpoint salvage
    if metrics.has_unsalvaged_errors() {
        violations.push(InvariantViolation {
            invariant: "checkpoint_salvage",
            severity: "error",
            description: format!(
                "Turns {:?} had errors without a preceding checkpoint",
                metrics.unsalvaged_error_turns
            ),
        });
    }

    // 2. Approval reduction
    let max_approvals = metrics.max_approvals_per_turn();
    if max_approvals > config.max_approvals_per_turn {
        violations.push(InvariantViolation {
            invariant: "approval_cap",
            severity: "warning",
            description: format!(
                "Max approvals per turn ({}) exceeds cap ({})",
                max_approvals, config.max_approvals_per_turn
            ),
        });
    }

    // 3. Confidence health
    if metrics.max_consecutive_low_confidence > config.max_consecutive_low_confidence {
        violations.push(InvariantViolation {
            invariant: "confidence_health",
            severity: "warning",
            description: format!(
                "Confidence collapsed for {} consecutive turns (max allowed: {})",
                metrics.max_consecutive_low_confidence, config.max_consecutive_low_confidence
            ),
        });
    }

    // 4. Interruption recording
    if metrics.turn_error_count > 0 && metrics.interruption_count == 0 {
        violations.push(InvariantViolation {
            invariant: "interruption_recording",
            severity: "warning",
            description: format!(
                "{} turn errors but no InterruptionRecorded events",
                metrics.turn_error_count
            ),
        });
    }

    // 5. High tool failure turns
    if !metrics.high_failure_turns.is_empty() {
        violations.push(InvariantViolation {
            invariant: "tool_failure_rate",
            severity: "warning",
            description: format!(
                "Turns with >{}% tool failure rate: {:?}",
                (config.tool_failure_rate_threshold * 100.0) as u32,
                metrics
                    .high_failure_turns
                    .iter()
                    .map(|(t, r)| format!("T{}: {:.0}%", t, r * 100.0))
                    .collect::<Vec<_>>()
            ),
        });
    }

    // 6. Compaction effectiveness — compaction ran but didn't prevent a subsequent 413.
    if metrics.ineffective_compaction_count > config.max_ineffective_compactions {
        violations.push(InvariantViolation {
            invariant: "compaction_effectiveness",
            severity: "warning",
            description: format!(
                "{} compaction events were followed by another context-window error (max allowed: {})",
                metrics.ineffective_compaction_count, config.max_ineffective_compactions
            ),
        });
    }

    // 7. Stall cascade — too many stall/drift events indicate the model is stuck.
    if metrics.stall_count > config.max_stall_count {
        violations.push(InvariantViolation {
            invariant: "stall_cascade",
            severity: "warning",
            description: format!(
                "{} stall events detected (max allowed: {})",
                metrics.stall_count, config.max_stall_count
            ),
        });
    }

    violations
}

// ─── Convenience ──────────────────────────────────────────────────────────

/// Load a JSONL file, extract metrics, and check invariants.
///
/// Returns `(metrics, violations)`. Useful for one-liner regression tests.
pub fn replay_and_check(
    content: &str,
    config: &ReplayInvariantConfig,
) -> (SessionReplayMetrics, Vec<InvariantViolation>) {
    let events = parse_journal_lines(content);
    let metrics = extract_metrics(&events);
    let violations = check_invariants(&metrics, config);
    (metrics, violations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_event(event_type: JournalEventType) -> JournalEvent {
        serde_json::from_value(json!({
            "type": event_type,
            "ts": "2025-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn make_turn_event(turn: u32, confidence: Option<f64>) -> JournalEvent {
        let mut e = make_event(JournalEventType::Turn);
        e.turn = Some(turn);
        e.selector_confidence = confidence;
        e
    }

    fn make_checkpoint_event(turn: u32) -> JournalEvent {
        let mut e = make_event(JournalEventType::Checkpoint);
        e.turn = Some(turn);
        e
    }

    fn make_error_event(turn: u32) -> JournalEvent {
        let mut e = make_event(JournalEventType::TurnError);
        e.turn = Some(turn);
        e.error = Some("test error".into());
        e
    }

    fn make_approval_event(turn: u32) -> JournalEvent {
        let mut e = make_event(JournalEventType::ApprovalRequired);
        e.turn = Some(turn);
        e
    }

    #[test]
    fn empty_session_has_no_violations() {
        let (metrics, violations) = replay_and_check("", &ReplayInvariantConfig::default());
        assert_eq!(metrics.total_events, 0);
        assert!(violations.is_empty());
    }

    #[test]
    fn checkpoint_salvage_violation() {
        let events = vec![
            make_turn_event(1, Some(0.5)),
            make_error_event(2), // error without checkpoint
        ];
        let metrics = extract_metrics(&events);
        assert!(metrics.has_unsalvaged_errors());
        assert_eq!(metrics.unsalvaged_error_turns, vec![2]);
    }

    #[test]
    fn checkpoint_salvage_satisfied() {
        let events = vec![
            make_turn_event(1, Some(0.5)),
            make_checkpoint_event(2),
            make_error_event(2), // error with checkpoint on same turn
        ];
        let metrics = extract_metrics(&events);
        assert!(!metrics.has_unsalvaged_errors());
    }

    #[test]
    fn approval_cap_violation() {
        let mut events = vec![make_turn_event(1, Some(0.5))];
        for _ in 0..20 {
            events.push(make_approval_event(1));
        }
        let metrics = extract_metrics(&events);
        assert_eq!(metrics.max_approvals_per_turn(), 20);

        let config = ReplayInvariantConfig {
            max_approvals_per_turn: 10,
            ..Default::default()
        };
        let violations = check_invariants(&metrics, &config);
        assert!(violations.iter().any(|v| v.invariant == "approval_cap"));
    }

    #[test]
    fn confidence_collapse_detection() {
        let events: Vec<JournalEvent> = (1..=8)
            .map(|t| make_turn_event(t, Some(0.3))) // all below floor
            .collect();
        let metrics = extract_metrics(&events);
        assert_eq!(metrics.max_consecutive_low_confidence, 8);

        let config = ReplayInvariantConfig {
            max_consecutive_low_confidence: 4,
            ..Default::default()
        };
        let violations = check_invariants(&metrics, &config);
        assert!(
            violations
                .iter()
                .any(|v| v.invariant == "confidence_health")
        );
    }

    #[test]
    fn confidence_recovers_resets_count() {
        let events = vec![
            make_turn_event(1, Some(0.3)),
            make_turn_event(2, Some(0.3)),
            make_turn_event(3, Some(0.7)), // recovery
            make_turn_event(4, Some(0.3)),
        ];
        let metrics = extract_metrics(&events);
        assert_eq!(metrics.max_consecutive_low_confidence, 2);
    }

    #[test]
    fn interruption_recording_violation() {
        let events = vec![make_error_event(1)]; // error without interruption
        let metrics = extract_metrics(&events);
        let violations = check_invariants(&metrics, &ReplayInvariantConfig::default());
        assert!(
            violations
                .iter()
                .any(|v| v.invariant == "interruption_recording")
        );
    }

    #[test]
    fn parse_journal_lines_skips_malformed() {
        let content = r#"{"type":"turn","ts":"2025-01-01T00:00:00Z","turn":1}
not valid json
{"type":"checkpoint","ts":"2025-01-01T00:00:01Z","turn":1}
"#;
        let events = parse_journal_lines(content);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn avg_confidence_computes() {
        let events = vec![
            make_turn_event(1, Some(0.4)),
            make_turn_event(2, Some(0.6)),
            make_turn_event(3, Some(0.8)),
        ];
        let metrics = extract_metrics(&events);
        let avg = metrics.avg_confidence();
        assert!((avg - 0.6).abs() < 0.01);
    }

    #[test]
    fn replay_and_check_integration() {
        // Build a JSONL string with known issues.
        let lines: Vec<String> = vec![
            serde_json::to_string(&make_turn_event(1, Some(0.3))).unwrap(),
            serde_json::to_string(&make_turn_event(2, Some(0.3))).unwrap(),
            serde_json::to_string(&make_turn_event(3, Some(0.3))).unwrap(),
            serde_json::to_string(&make_turn_event(4, Some(0.3))).unwrap(),
            serde_json::to_string(&make_turn_event(5, Some(0.3))).unwrap(),
            serde_json::to_string(&make_error_event(5)).unwrap(),
        ];
        let content = lines.join("\n");
        let config = ReplayInvariantConfig {
            max_consecutive_low_confidence: 3,
            ..Default::default()
        };
        let (metrics, violations) = replay_and_check(&content, &config);
        assert_eq!(metrics.turn_count, 5);
        assert_eq!(metrics.turn_error_count, 1);
        assert!(metrics.has_unsalvaged_errors());
        assert!(violations.len() >= 2); // checkpoint + confidence + interruption
    }

    fn make_compact_event(turn: u32) -> JournalEvent {
        let mut e = make_event(JournalEventType::Compact);
        e.turn = Some(turn);
        e
    }

    fn make_context_error_event(turn: u32) -> JournalEvent {
        let mut e = make_event(JournalEventType::TurnError);
        e.turn = Some(turn);
        e.error = Some("context_window overflow: prompt is too long".into());
        e
    }

    fn make_stall_event(turn: u32) -> JournalEvent {
        let mut e = make_event(JournalEventType::StallDetected);
        e.turn = Some(turn);
        e
    }

    fn make_turn_with_tokens(turn: u32, tokens_in: u64, tokens_out: u64) -> JournalEvent {
        let mut e = make_turn_event(turn, Some(0.5));
        e.tokens_in = Some(tokens_in);
        e.tokens_out = Some(tokens_out);
        e
    }

    #[test]
    fn compaction_effectiveness_invariant() {
        // Compact on turn 2, then another context-window error on same turn.
        let events = vec![
            make_turn_event(1, Some(0.5)),
            make_compact_event(2),
            make_context_error_event(2),
            make_compact_event(3),
            make_context_error_event(3),
            make_compact_event(4),
            make_context_error_event(4),
        ];
        let metrics = extract_metrics(&events);
        assert_eq!(metrics.compact_count, 3);
        assert_eq!(metrics.context_window_error_count, 3);
        assert_eq!(metrics.ineffective_compaction_count, 3);

        let config = ReplayInvariantConfig {
            max_ineffective_compactions: 2,
            ..Default::default()
        };
        let violations = check_invariants(&metrics, &config);
        assert!(
            violations
                .iter()
                .any(|v| v.invariant == "compaction_effectiveness")
        );
    }

    #[test]
    fn effective_compaction_no_violation() {
        // Compact on turn 2, error on turn 3 (different turn) — not ineffective.
        let events = vec![
            make_turn_event(1, Some(0.5)),
            make_compact_event(2),
            make_context_error_event(3),
        ];
        let metrics = extract_metrics(&events);
        assert_eq!(metrics.compact_count, 1);
        assert_eq!(metrics.context_window_error_count, 1);
        assert_eq!(metrics.ineffective_compaction_count, 0);
    }

    #[test]
    fn stall_cascade_invariant() {
        let events = vec![
            make_turn_event(1, Some(0.5)),
            make_stall_event(1),
            make_stall_event(2),
            make_stall_event(3),
            make_stall_event(4),
        ];
        let metrics = extract_metrics(&events);
        assert_eq!(metrics.stall_count, 4);

        let config = ReplayInvariantConfig {
            max_stall_count: 3,
            ..Default::default()
        };
        let violations = check_invariants(&metrics, &config);
        assert!(violations.iter().any(|v| v.invariant == "stall_cascade"));
    }

    #[test]
    fn token_usage_tracking() {
        let events = vec![
            make_turn_with_tokens(1, 1000, 500),
            make_turn_with_tokens(2, 2000, 800),
            make_turn_with_tokens(3, 1500, 700),
        ];
        let metrics = extract_metrics(&events);
        assert_eq!(metrics.total_tokens_in, 4500);
        assert_eq!(metrics.total_tokens_out, 2000);
    }

    #[test]
    fn context_window_error_detected_from_error_msg() {
        let events = vec![
            make_context_error_event(1),
            make_error_event(2), // regular error, not context-window
        ];
        let metrics = extract_metrics(&events);
        assert_eq!(metrics.context_window_error_count, 1);
        assert_eq!(metrics.turn_error_count, 2);
    }
}

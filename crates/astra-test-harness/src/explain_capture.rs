//! Bounded archival of canonical facts, not another token accounting reducer.
use astra_turn_types::{ExplainAnalyzeEventV1, decode_explain_analyze_wire};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_FACTS: usize = 4096;
pub(crate) const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Per-subprocess evidence retained before temporary files/session cleanup.
/// Consumers must use the canonical graph's scope/usage conflict checks in
/// addition to these transport diagnostics. Empty facts never certify zero use.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExplainCapture {
    pub events: Vec<ExplainAnalyzeEventV1>,
    pub diagnostics: Vec<String>,
    pub snapshot_pending: bool,
    pub gap_unrecovered: bool,
    pub identity_verified: bool,
    #[serde(skip)]
    retained_bytes: usize,
}

impl ExplainCapture {
    pub(crate) fn primary_prompt_cache_usage(
        &self,
    ) -> Option<astra_turn_types::NormalizedPromptCacheUsage> {
        self.canonical_graph()?.primary_prompt_cache_usage()
    }

    pub(crate) fn canonical_graph(&self) -> Option<astra_turn_types::ExplainAnalyzeGraphV1> {
        if !self.identity_verified
            || !self.diagnostics.is_empty()
            || self.snapshot_pending
            || self.gap_unrecovered
        {
            return None;
        }
        let mut graph = astra_turn_types::ExplainAnalyzeGraphV1::default();
        for event in &self.events {
            graph.apply(event.clone());
        }
        graph.finish_ingest();
        Some(graph)
    }

    pub(crate) fn diagnose(&mut self, code: &str) {
        if !self.diagnostics.iter().any(|existing| existing == code) {
            self.diagnostics.push(code.into());
        }
    }

    fn retain(&mut self, fact: ExplainAnalyzeEventV1) -> bool {
        if !fact.is_valid() {
            self.diagnose("invalid_fact");
            return false;
        }
        // Exact live/snapshot copies need no second archive slot. Conflicting
        // facts remain present for the canonical reducer to diagnose; a later
        // snapshot must not erase the evidence of a disagreement.
        if self.events.contains(&fact) {
            return true;
        }
        if self.retained_bytes == 0 && !self.events.is_empty() {
            self.retained_bytes = self
                .events
                .iter()
                .map(|event| {
                    serde_json::to_vec(event)
                        .expect("typed fact serializes")
                        .len()
                })
                .sum();
        }
        let bytes = serde_json::to_vec(&fact)
            .expect("typed fact serializes")
            .len();
        if self.events.len() >= MAX_FACTS || self.retained_bytes.saturating_add(bytes) > MAX_BYTES {
            self.diagnose("capture_truncated");
            return false;
        }
        self.retained_bytes += bytes;
        self.events.push(fact);
        true
    }

    pub(crate) fn observe(&mut self, wire: &Value) {
        match wire.get("type").and_then(Value::as_str) {
            Some("explain_analyze") => {
                self.snapshot_pending = true;
                match decode_explain_analyze_wire(wire) {
                    Ok(fact) => {
                        self.retain(fact);
                    }
                    Err(_) => self.diagnose("invalid_fact"),
                }
            }
            Some("explain_analyze_snapshot") => {
                let (Some(events), Some(degraded)) = (
                    wire.get("events").and_then(Value::as_array),
                    wire.get("delivery_degraded").and_then(Value::as_bool),
                ) else {
                    self.diagnose("invalid_snapshot");
                    return;
                };
                if events.is_empty() {
                    self.diagnose("empty_snapshot");
                    return;
                }
                let mut recovered = !degraded;
                for value in events {
                    match serde_json::from_value::<ExplainAnalyzeEventV1>(value.clone()) {
                        Ok(fact) => recovered &= self.retain(fact),
                        Err(_) => {
                            self.diagnose("invalid_fact");
                            recovered = false;
                        }
                    }
                }
                if degraded {
                    self.diagnose("delivery_degraded");
                }
                if recovered {
                    self.snapshot_pending = false;
                    self.gap_unrecovered = false;
                }
            }
            Some("stream_gap")
                if wire.get("explain_analyze_recovered") == Some(&Value::Bool(false)) =>
            {
                self.gap_unrecovered = true;
            }
            _ => {}
        }
    }

    pub(crate) fn bind(&mut self, run_id: Option<&str>) {
        self.identity_verified = run_id.is_some_and(|id| {
            !self.events.is_empty() && self.events.iter().all(|event| event.run_id == id)
        });
        if !self.identity_verified {
            self.diagnose("unverified_run_scope");
        }
    }
}

/// Preserve trusted subprocess order; never infer ordering from clock IDs.
/// Every execution is qualified before the caller applies warmup.
/// Outer groups are user turns, additionally split by run for the intra-run
/// hard gate. Inner groups are requests, or one checked total per user turn.
pub(crate) fn primary_execution_cache_groups(
    executions: &[&crate::runner::RunOutcome],
    rounds: bool,
    within_run: bool,
) -> Option<Vec<Vec<astra_turn_types::NormalizedPromptCacheUsage>>> {
    use astra_turn_types::NormalizedPromptCacheUsage;
    if executions.is_empty() || (within_run && !rounds) {
        return None;
    }
    if executions.len() > 1 {
        let session = executions[0]
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())?;
        if executions
            .iter()
            .any(|execution| execution.session_id.as_deref() != Some(session))
        {
            return None;
        }
    }
    let mut scopes = std::collections::HashSet::new();
    let mut seen_turns = std::collections::HashSet::new();
    let mut last_turn = None;
    let mut last_run = None;
    let mut seen_groups = std::collections::HashSet::new();
    let mut groups = Vec::<Vec<NormalizedPromptCacheUsage>>::new();
    for execution in executions {
        let capture = execution.explain_capture.as_ref()?;
        if capture
            .events
            .iter()
            .any(|event| execution.run_id.as_deref() != Some(event.run_id.as_str()))
        {
            return None;
        }
        let graph = capture.canonical_graph()?;
        let mut coverage = graph.execution_scope_coverage();
        if coverage.len() != 1 {
            return None;
        }
        let scope = coverage.pop()?;
        if !scopes.insert((
            scope.run_id.clone(),
            scope.turn_id.clone(),
            scope.clock_domain_id,
        )) {
            return None;
        }
        let same_turn = last_turn.as_ref() == Some(&scope.turn_id);
        if !same_turn && !seen_turns.insert(scope.turn_id.clone()) {
            return None;
        }
        let same_group = same_turn && (!within_run || last_run.as_ref() == Some(&scope.run_id));
        if within_run
            && !same_group
            && !seen_groups.insert((scope.turn_id.clone(), scope.run_id.clone()))
        {
            return None;
        }
        last_turn = Some(scope.turn_id);
        last_run = Some(scope.run_id);
        if rounds {
            let requests = graph.primary_prompt_cache_request_groups()?;
            if same_group {
                groups.last_mut()?.extend(requests);
            } else {
                groups.push(requests);
            }
        } else {
            let usage = graph.primary_prompt_cache_usage()?;
            if same_turn {
                let total = groups.last_mut()?.first_mut()?;
                total.fresh_input_tokens = total
                    .fresh_input_tokens
                    .checked_add(usage.fresh_input_tokens)?;
                total.cache_read_tokens = total
                    .cache_read_tokens
                    .checked_add(usage.cache_read_tokens)?;
                total.cache_creation_tokens = total
                    .cache_creation_tokens
                    .checked_add(usage.cache_creation_tokens)?;
                total.checked_total_input_tokens()?;
            } else {
                groups.push(vec![usage]);
            }
        }
    }
    Some(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire(id: &str, run: &str) -> Value {
        json!({"type":"explain_analyze","schema_version":1,"event_id":id,
            "run_id":run,"turn_id":"t","node_id":id,"producer_id":"p",
            "clock_domain_id":"c","kind":"admission","label":"Admission",
            "transition":"started","elapsed_ms":0})
    }

    fn snapshot(facts: &[Value], degraded: bool) -> Value {
        let facts: Vec<_> = facts
            .iter()
            .map(|fact| decode_explain_analyze_wire(fact).unwrap())
            .collect();
        json!({"type":"explain_analyze_snapshot","events":facts,"delivery_degraded":degraded})
    }

    #[test]
    fn snapshots_preserve_earlier_exchange_and_deduplicate_exact_copies() {
        let first = wire("first", "r");
        let second = wire("second", "r");
        let mut capture = ExplainCapture::default();
        capture.observe(&first);
        capture.observe(&snapshot(&[first], false));
        capture.observe(&snapshot(&[second], false));
        capture.bind(Some("r"));
        assert_eq!(capture.events.len(), 2);
        assert!(capture.identity_verified);
        assert!(!capture.snapshot_pending);
        assert!(capture.diagnostics.is_empty());
        let restored: ExplainCapture =
            serde_json::from_value(serde_json::to_value(&capture).unwrap()).unwrap();
        assert_eq!(restored.events, capture.events);
    }

    #[test]
    fn recovery_clears_transport_gap_not_degradation_or_conflicting_facts() {
        let first = wire("first", "r");
        let mut changed = first.clone();
        changed["label"] = json!("Conflicting label");
        let mut capture = ExplainCapture::default();
        capture.observe(&snapshot(&[first], true));
        capture.observe(&json!({"type":"stream_gap","explain_analyze_recovered":false}));
        assert!(capture.gap_unrecovered);
        capture.observe(&snapshot(&[changed], false));
        assert!(!capture.gap_unrecovered);
        assert_eq!(
            capture.events.len(),
            2,
            "retain disagreement for canonical reducer"
        );
        assert_eq!(capture.diagnostics, ["delivery_degraded"]);
    }

    #[test]
    fn foreign_or_missing_run_and_invalid_snapshot_are_not_certified() {
        let mut capture = ExplainCapture::default();
        capture.observe(&wire("first", "foreign"));
        capture.observe(&json!({"type":"explain_analyze_snapshot","events":[]}));
        capture.bind(Some("root"));
        assert!(!capture.identity_verified);
        assert!(capture.snapshot_pending);
        assert!(capture.diagnostics.contains(&"invalid_snapshot".into()));
        capture.bind(None);
        assert!(!capture.identity_verified);
    }

    #[test]
    fn over_limit_keeps_known_facts_and_explicit_loss() {
        let mut capture = ExplainCapture::default();
        for id in 0..=MAX_FACTS {
            capture.observe(&wire(&format!("event-{id}"), "r"));
        }
        assert_eq!(capture.events.len(), MAX_FACTS);
        assert_eq!(capture.diagnostics, ["capture_truncated"]);
        capture.observe(&snapshot(&[], false));
        assert!(capture.diagnostics.contains(&"capture_truncated".into()));
    }

    #[test]
    fn actual_byte_limit_and_invalid_snapshot_leave_gap_unrecovered() {
        let mut capture = ExplainCapture::default();
        for id in 0..MAX_FACTS {
            let mut event = wire(&format!("event-{id}"), "r");
            for key in [
                "run_id",
                "turn_id",
                "node_id",
                "producer_id",
                "clock_domain_id",
            ] {
                event[key] = json!("x".repeat(512));
            }
            capture.observe(&event);
            if capture.diagnostics.contains(&"capture_truncated".into()) {
                break;
            }
        }
        assert!(capture.events.len() < MAX_FACTS);
        assert!(capture.retained_bytes <= MAX_BYTES);
        assert!(capture.diagnostics.contains(&"capture_truncated".into()));
        capture.observe(&json!({"type":"stream_gap","explain_analyze_recovered":false}));
        capture.observe(
            &json!({"type":"explain_analyze_snapshot","events":[{}],"delivery_degraded":false}),
        );
        assert!(capture.gap_unrecovered);
        assert!(capture.snapshot_pending);
    }

    #[test]
    fn archived_usage_replays_through_canonical_reducer_without_double_counting() {
        let mut event = wire("terminal", "r");
        event["kind"] = json!("turn");
        event["transition"] = json!("finished");
        event["outcome"] = json!("completed");
        event["start_elapsed_ms"] = json!(0);
        event["duration_ms"] = json!(0);
        event["auxiliary_usage"] = json!({"available":true,"truncated":false,"attempts":[{
            "attempt_id":"aux-1","provider":"typesafe","offering_id":"offering",
            "model_name":"jev","purpose":"verification_judge","operation_id":"judge",
            "usage_status":"provider_exact","usage":{"basis":"provider_exact",
                "fresh_input_tokens":100,"output_tokens":10}
        }]});
        let mut capture = ExplainCapture::default();
        capture.observe(&event);
        capture.observe(&snapshot(&[event], false));
        let restored: ExplainCapture =
            serde_json::from_value(serde_json::to_value(capture).unwrap()).unwrap();
        let mut graph = astra_turn_types::ExplainAnalyzeGraphV1::default();
        for fact in restored.events {
            graph.apply(fact);
        }
        graph.finish_ingest();
        let usage = graph.auxiliary_usage_snapshot();
        assert!(usage.available);
        assert_eq!(usage.attempts.len(), 1);
        assert_eq!(usage.attempts[0].model_name, "jev");
        assert_eq!(
            usage.attempts[0].usage.as_ref().unwrap().fresh_input_tokens,
            Some(100)
        );
        assert_eq!(
            usage.attempts[0].usage.as_ref().unwrap().cache_read_tokens,
            None
        );
    }
}

use super::ReflectReport;
use astra_core::ObservationDepth;
use std::collections::BTreeSet;

impl ReflectReport {
    /// Bound routine reflection without changing explicit diagnostic reports.
    /// This is presentation only: source evidence and identifiers stay intact.
    pub fn project_lightweight(mut self) -> Self {
        let depth = ObservationDepth::from_arg(&self.depth);
        if !matches!(depth, ObservationDepth::Hint | ObservationDepth::Summary) {
            return self;
        }
        let (max_observations, max_evidence, max_hints) = depth.report_limits();
        let before = (
            self.observations.len(),
            self.evidence.len(),
            self.action_hints.len(),
            self.failure_clusters.len(),
            self.graph_slice.nodes.len(),
            self.graph_slice.edges.len(),
        );
        // Severity is a structured report field, not inferred from prose.
        self.observations
            .sort_by_key(|observation| match observation.severity.as_str() {
                "critical" => 0,
                "error" => 1,
                "warning" => 2,
                _ => 3,
            });
        let available: BTreeSet<_> = self.evidence.iter().map(|e| e.ref_id.clone()).collect();
        let mut selected = BTreeSet::new();
        let mut kept = 0;
        self.observations.retain(|observation| {
            if kept == max_observations {
                return false;
            }
            if !observation.evidence_refs.is_empty() {
                let support = observation
                    .evidence_refs
                    .iter()
                    .find(|id| selected.contains(*id))
                    .or_else(|| {
                        (selected.len() < max_evidence)
                            .then(|| {
                                observation
                                    .evidence_refs
                                    .iter()
                                    .find(|id| available.contains(*id))
                            })
                            .flatten()
                    });
                let Some(support) = support else {
                    return false;
                };
                selected.insert(support.clone());
            }
            kept += 1;
            true
        });
        for observation in &self.observations {
            for id in &observation.evidence_refs {
                if selected.len() < max_evidence && available.contains(id) {
                    selected.insert(id.clone());
                }
            }
        }
        self.evidence.retain(|e| selected.contains(&e.ref_id));
        for observation in &mut self.observations {
            observation.evidence_refs.retain(|id| selected.contains(id));
        }
        let retained: BTreeSet<_> = self.observations.iter().map(|o| o.ref_id.clone()).collect();
        self.action_hints.retain(|hint| {
            !hint.observation_refs.is_empty()
                && hint.observation_refs.iter().all(|id| retained.contains(id))
        });
        self.action_hints.sort_by_key(|hint| {
            self.observations
                .iter()
                .position(|observation| hint.observation_refs.contains(&observation.ref_id))
                .unwrap_or(usize::MAX)
        });
        self.action_hints.truncate(max_hints);
        self.failure_clusters.retain(|cluster| {
            !cluster.observation_refs.is_empty()
                && cluster
                    .observation_refs
                    .iter()
                    .all(|id| retained.contains(id))
        });
        self.failure_clusters.truncate(max_observations);
        self.graph_slice.nodes.clear();
        self.graph_slice.edges.clear();
        let text_limit = if depth == ObservationDepth::Hint {
            180
        } else {
            360
        };
        let mut shortened = false;
        let mut bound = |text: &mut String| {
            if let Some((end, _)) = text.char_indices().nth(text_limit) {
                text.truncate(end);
                text.push('…');
                shortened = true;
            }
        };
        bound(&mut self.summary);
        for observation in &mut self.observations {
            bound(&mut observation.summary);
        }
        for evidence in &mut self.evidence {
            bound(&mut evidence.summary);
        }
        for hint in &mut self.action_hints {
            bound(&mut hint.summary);
        }
        for cluster in &mut self.failure_clusters {
            bound(&mut cluster.summary);
            bound(&mut cluster.label);
        }
        let omitted = &mut self.budget_result.omitted;
        omitted.observations += before.0.saturating_sub(self.observations.len()) as i64;
        omitted.evidence_previews += before.1.saturating_sub(self.evidence.len()) as i64;
        omitted.action_hints += before.2.saturating_sub(self.action_hints.len()) as i64;
        omitted.nodes += before.4 as i64;
        self.budget_result.truncated |= shortened
            || !omitted.is_empty()
            || before.3 != self.failure_clusters.len()
            || before.5 > 0;
        self.graph_slice.budget_result = self.budget_result.clone();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn large_report(depth: &str) -> ReflectReport {
        let text = "证据🧪".repeat(2000);
        let observations: Vec<_> = (0..60)
            .map(|i| {
                json!({
                    "ref_id":format!("obs-{i}"),"topic":"execution","facet":"errors",
                    "kind":"diagnosis","severity":if i==59 {"critical"} else {"info"},
                    "summary":text,"confidence":{"evidence":0.8},"evidence_refs":[format!("ev-{i}")]
                })
            })
            .collect();
        let evidence: Vec<_> = (0..60)
            .map(|i| {
                json!({
                    "ref_id":format!("ev-{i}"),"evidence_class":"observed_evidence","source":"test",
                    "summary":text,"confidence":{"evidence":0.8}
                })
            })
            .collect();
        let nodes: Vec<_> = (0..60).map(|i| json!({
            "ref_id":format!("ev-{i}"),"layer":"runtime","kind":"event","label":"event","summary":text
        })).collect();
        serde_json::from_value(json!({
            "schema_version":1,"tool":"reflect","session_id":"session","analysis_view":"overview",
            "topic":"overview","facet":"overview","depth":depth,"horizon":"session",
            "source_policy":"auto","include_context":false,"summary":text,
            "data_coverage":{"overall":"fresh","source":"test","events":60,"decisions":0},
            "observations":observations,"evidence":evidence,
            "action_hints":[{"target_type":"tool","summary":text,"confidence":{},"observation_refs":["obs-59"]}],
            "graph_slice":{"nodes":nodes,"edges":[]}
        })).unwrap()
    }

    #[test]
    fn lightweight_reflection_prioritizes_actions_and_drops_missing_support() {
        let mut report = large_report("summary");
        let critical_hint = report.action_hints[0].clone();
        report.action_hints = (0..8)
            .map(|_| {
                let mut hint = critical_hint.clone();
                hint.observation_refs = vec!["obs-0".into()];
                hint
            })
            .collect();
        report.action_hints.push(critical_hint.clone());
        let projected = report.clone().project_lightweight();
        assert_eq!(projected.action_hints[0].observation_refs, vec!["obs-59"]);
        assert_eq!(projected.action_hints.len(), 4);
        assert_eq!(
            serde_json::to_vec(&projected).unwrap(),
            serde_json::to_vec(&projected.clone().project_lightweight()).unwrap()
        );
        report.evidence.retain(|e| e.ref_id != "ev-59");
        let unsupported = report.project_lightweight();
        assert!(
            !unsupported
                .observations
                .iter()
                .any(|o| o.ref_id == "obs-59")
        );
        assert!(
            !unsupported
                .action_hints
                .iter()
                .any(|hint| hint.observation_refs.contains(&"obs-59".into()))
        );
        assert!(unsupported.budget_result.truncated);
    }

    #[test]
    fn lightweight_reflection_keeps_critical_support_and_bounds_long_history() {
        let summary = large_report("summary").project_lightweight();
        assert_eq!(summary.observations[0].ref_id, "obs-59");
        assert!(summary.evidence.iter().any(|e| e.ref_id == "ev-59"));
        assert_eq!(summary.action_hints.len(), 1);
        let evidence: BTreeSet<_> = summary.evidence.iter().map(|e| &e.ref_id).collect();
        assert!(
            summary
                .observations
                .iter()
                .all(|o| o.evidence_refs.iter().all(|id| evidence.contains(id)))
        );
        assert!(summary.graph_slice.nodes.is_empty());
        assert!(summary.budget_result.truncated);
        assert_eq!(summary.budget_result.omitted.observations, 56);
        assert_eq!(summary.budget_result.omitted.evidence_previews, 56);
        assert_eq!(summary.budget_result.omitted.nodes, 60);
        let bytes = serde_json::to_vec(&summary).unwrap();
        assert!(bytes.len() < 20_000, "{} bytes", bytes.len());
        assert_eq!(summary.clone().project_lightweight(), summary);
        let hint = large_report("hint").project_lightweight();
        assert!(serde_json::to_vec(&hint).unwrap().len() < bytes.len());
        for depth in ["diagnostic", "forensic"] {
            let report = large_report(depth);
            assert_eq!(report.clone().project_lightweight(), report);
        }
    }
}

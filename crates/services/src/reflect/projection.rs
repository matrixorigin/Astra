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
        let model_request_groups_before_projection = self
            .model_requests
            .terminal
            .as_ref()
            .map_or(0, |requests| requests.groups.len());
        if let Some(usage) = self.judgment_usage.as_mut() {
            let group_limit = if depth == ObservationDepth::Hint {
                2
            } else {
                8
            };
            usage.omitted_groups += usage.groups.len().saturating_sub(group_limit);
            usage.groups.truncate(group_limit);
        }
        if let Some(requests) = self.model_requests.terminal.as_mut() {
            let group_limit = if depth == ObservationDepth::Hint {
                2
            } else {
                8
            };
            requests.omitted_groups += requests.groups.len().saturating_sub(group_limit);
            requests.groups.truncate(group_limit);
        }
        let (max_observations, _, _) = depth.report_limits();
        let before = (
            self.observations.len(),
            self.evidence.len(),
            self.action_hints.len(),
            self.failure_clusters.len(),
            self.graph_slice.nodes.len(),
            self.graph_slice.edges.len(),
        );
        let preferred = self
            .graph_slice
            .nodes
            .iter()
            .filter(|node| super::is_execution_spine(node))
            .map(|node| node.ref_id.clone())
            .collect();
        astra_core::budget_observation_support(
            depth,
            &mut self.observations,
            &mut self.evidence,
            &mut self.action_hints,
            &preferred,
        );
        let selected: BTreeSet<_> = self.evidence.iter().map(|item| &item.ref_id).collect();
        let retained: BTreeSet<_> = self.observations.iter().map(|o| o.ref_id.clone()).collect();
        self.failure_clusters.retain(|cluster| {
            !cluster.observation_refs.is_empty()
                && cluster
                    .observation_refs
                    .iter()
                    .all(|id| retained.contains(id))
        });
        self.failure_clusters.truncate(max_observations);
        self.graph_slice
            .nodes
            .retain(|node| super::is_execution_spine(node) && selected.contains(&node.ref_id));
        let node_refs: BTreeSet<_> = self
            .graph_slice
            .nodes
            .iter()
            .map(|node| &node.ref_id)
            .collect();
        self.graph_slice
            .edges
            .retain(|edge| node_refs.contains(&edge.from) && node_refs.contains(&edge.to));
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
        for node in &mut self.graph_slice.nodes {
            if let Some(summary) = node.summary.as_mut() {
                bound(summary);
            }
            bound(&mut node.label);
        }
        let omitted = &mut self.budget_result.omitted;
        omitted.observations += before.0.saturating_sub(self.observations.len()) as i64;
        omitted.evidence_previews += before.1.saturating_sub(self.evidence.len()) as i64;
        omitted.action_hints += before.2.saturating_sub(self.action_hints.len()) as i64;
        omitted.nodes += before.4.saturating_sub(self.graph_slice.nodes.len()) as i64;
        self.budget_result.truncated |= shortened
            || !omitted.is_empty()
            || before.3 != self.failure_clusters.len()
            || before.5 != self.graph_slice.edges.len()
            || model_request_groups_before_projection
                != self
                    .model_requests
                    .terminal
                    .as_ref()
                    .map_or(0, |requests| requests.groups.len());
        self.graph_slice.budget_result = self.budget_result.clone();
        // Include graph envelopes and the rest of the final report, not just
        // metadata. Discard only complete facts and account for every omission.
        loop {
            let graph_over_budget = serde_json::to_vec(&self.graph_slice.nodes)
                .expect("graph nodes")
                .len()
                > 8192;
            let report_over_budget =
                serde_json::to_vec(&self).expect("reflect report").len() >= 20_000;
            if !graph_over_budget && !report_over_budget {
                break;
            }
            if report_over_budget
                && let Some(requests) = self.model_requests.terminal.as_mut()
                && requests.groups.pop().is_some()
            {
                requests.omitted_groups = requests.omitted_groups.saturating_add(1);
                self.budget_result.truncated = true;
                self.graph_slice.budget_result = self.budget_result.clone();
                continue;
            }
            let Some(spine) = self
                .graph_slice
                .nodes
                .iter_mut()
                .rev()
                .filter_map(|node| node.metadata.as_mut()?.get_mut("execution_spine"))
                .find(|spine| {
                    spine["facts"]
                        .as_array()
                        .is_some_and(|facts| !facts.is_empty())
                })
            else {
                break;
            };
            spine["facts"]
                .as_array_mut()
                .expect("nonempty facts")
                .remove(0);
            spine["omitted_facts"] = (spine["omitted_facts"].as_u64().unwrap_or(0) + 1).into();
            spine["truncated"] = true.into();
            self.budget_result.truncated = true;
            self.graph_slice.budget_result = self.budget_result.clone();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reflect::{JudgmentUsageGroup, JudgmentUsageSummary};
    use serde_json::json;

    #[test]
    fn canonical_lifecycle_events_keep_top_level_identity() {
        use super::super::EvidenceEvent;
        // Canonical TraceEvent carries identity in columns, not duplicated in
        // metadata. These are the event names emitted by the runtime spawner.
        for (kind, status) in [
            ("agent_spawned", "spawned"),
            ("agent_completed", "completed"),
            ("agent_failed", "failed"),
            ("agent_cancelled", "cancelled"),
            ("agent_interrupted", "interrupted"),
        ] {
            let event = EvidenceEvent {
                event_id: format!("trace-{kind}"),
                event_type: kind.into(),
                run_id: Some("child-run".into()),
                parent_run_id: Some("parent-run".into()),
                agent_id: Some("child-agent".into()),
                metadata: Some(json!({"status":status})),
                ..Default::default()
            };
            let fact = event.execution_fact().expect("captured lifecycle fact");
            assert_eq!(fact.outcome.as_deref(), Some(status));
            assert!(fact.metadata_available);
            assert_eq!(fact.parent_run_id.as_deref(), Some("parent-run"));
            assert_eq!(fact.agent_id.as_deref(), Some("child-agent"));
            let mut conflicting = event;
            conflicting.metadata = Some(json!({"status":status, "agent_id":"other-agent"}));
            let rejected = conflicting.execution_fact().unwrap();
            assert!(!rejected.metadata_available);
            assert!(rejected.outcome.is_none());
            assert!(rejected.parent_run_id.is_none());
        }
    }

    #[test]
    fn captured_tool_outcomes_keep_scope_and_non_execution() {
        use super::super::EvidenceEvent;
        use crate::session_journal::ToolCallDisposition;
        for (disposition, ok) in [
            (ToolCallDisposition::Executed, true),
            (ToolCallDisposition::Executed, false),
            (ToolCallDisposition::Reused, true),
            (ToolCallDisposition::Suppressed, true),
            (ToolCallDisposition::Rejected, false),
            (ToolCallDisposition::Deferred, true),
        ] {
            let event = EvidenceEvent {
                event_id: "child-tool".into(),
                event_type: disposition.terminal_event_type(ok).into(),
                run_id: Some("child-run".into()),
                parent_run_id: Some("parent-run".into()),
                agent_id: Some("child-agent".into()),
                tool_call_id: Some("child-call".into()),
                skill_name: Some("bash".into()),
                metadata: Some(json!({"ok":ok,"disposition":disposition,
                    "result_class":"test_failure","exit_semantics":"domain_negative",
                    "result_preview":"PRIVATE-OUTPUT","args_preview":"PRIVATE-INPUT"})),
                ..Default::default()
            };
            let wire = serde_json::to_value(project_events(std::slice::from_ref(&event))).unwrap();
            let spine = &wire["graph_slice"]["nodes"][0]["metadata"]["execution_spine"];
            let fact = &spine["facts"][0];
            assert_eq!(spine["run_id"], "parent-run");
            assert_eq!(fact["run_id"], "child-run");
            assert_eq!(fact["tool_call_id"], "child-call");
            assert_eq!(fact["tool_outcome"]["disposition"], json!(disposition));
            assert_eq!(fact["tool_outcome"]["ok"], ok);
            assert_eq!(fact["tool_outcome"]["result_class"], "test_failure");
            assert_eq!(spine["task_completion"], "not_established");
            assert!(!wire.to_string().contains("PRIVATE-"));
            let mut conflicting = event;
            conflicting.metadata.as_mut().unwrap()["disposition"] =
                json!(if disposition == ToolCallDisposition::Executed {
                    ToolCallDisposition::Reused
                } else {
                    ToolCallDisposition::Executed
                });
            let rejected = conflicting.execution_fact().unwrap();
            assert!(rejected.tool_outcome.is_none());
            assert!(!rejected.metadata_available);
        }
        let event_without_canonical_call_id = EvidenceEvent {
            event_type: "tool_call_failed".into(),
            run_id: Some("child-run".into()),
            parent_run_id: Some("parent-run".into()),
            agent_id: Some("child-agent".into()),
            skill_name: Some("bash".into()),
            metadata: Some(json!({
                "ok": false,
                "disposition": ToolCallDisposition::Executed,
                "attrs": {"tool_call_id": "noncanonical-call"}
            })),
            ..Default::default()
        };
        let fact = event_without_canonical_call_id.execution_fact().unwrap();
        assert!(fact.tool_outcome.is_none());
        assert!(!fact.metadata_available);
    }

    #[test]
    fn default_summary_keeps_observed_adoption_not_guessed_completion() {
        use super::super::EvidenceEvent;
        for (outcome, producer, omitted) in [
            ("results_adopted", "parent-run", false),
            ("observation_timed_out", "parent-run", false),
            ("results_adopted", "other-run", false),
            ("results_adopted", "parent-run", true),
        ] {
            let mut events: Vec<_> = (0..80)
                .map(|index| EvidenceEvent {
                    event_id: format!("noise-{index}"),
                    event_type: "session_memory_extraction".into(),
                    content: "routine memory detail".into(),
                    ..Default::default()
                })
                .collect();
            events.push(EvidenceEvent {
                event_id: "dependency-fact".into(), event_type: "trace_span".into(),
                run_id: Some(producer.into()), metadata_omitted: omitted,
                metadata: (!omitted).then(|| json!({"name":"agent_dependency_boundary", "attrs": {
                    "parent_run_id":"parent-run", "outcome":outcome,
                    "tool_call_id":"wait-call",
                    "children":serde_json::to_string(&json!([{"agent_id":"child", "run_id":"child-run", "status":"completed"}])).unwrap()
                }})), ..Default::default()
            });
            let projected = project_events(&events);
            let wire = serde_json::to_value(&projected).unwrap();
            let spine = &wire["graph_slice"]["nodes"][0]["metadata"]["execution_spine"];
            assert_eq!(spine["run_id"], producer);
            if producer == "parent-run" && !omitted {
                assert_eq!(spine["facts"][0]["outcome"], outcome);
                assert_eq!(spine["facts"][0]["tool_call_id"], "wait-call");
            } else {
                assert!(spine["facts"][0]["outcome"].is_null());
                assert!(spine["facts"][0]["tool_call_id"].is_null());
                assert_eq!(spine["facts"][0]["metadata_available"], false);
                assert_eq!(spine["facts"][0]["children"], json!([]));
            }
            assert_eq!(spine["task_completion"], "not_established");
            assert_eq!(
                serde_json::to_value(projected.clone().project_lightweight()).unwrap(),
                wire
            );
        }
    }

    fn project_events(events: &[super::super::EvidenceEvent]) -> ReflectReport {
        use super::super::{ReflectRequest, SessionOverview};
        let (graph, budget) = super::super::budget_reflect_evidence_graph(
            super::super::build_evidence_graph(&[], events, &Default::default()),
        );
        let overview = SessionOverview {
            total_events: events.len() as i64,
            total_decisions: 0,
            duration_minutes: None,
            unique_skills_used: 0,
            error_count: 0,
            error_rate_pct: 0.0,
            top_event_types: vec![],
            top_skills: vec![],
        };
        let (_, observations, evidence, hints, clusters) = super::super::build_observation_envelope(
            "session",
            &ReflectRequest::from_observation_params(None, None, None, None, 10, ""),
            &overview,
            &[],
            &[],
            &[],
            graph.as_ref(),
        );
        let mut report = large_report("summary");
        report.observations = observations;
        report.evidence = evidence;
        report.action_hints = hints;
        report.failure_clusters = clusters;
        report.graph_slice = graph.unwrap();
        report.budget_result = budget;
        report.project_lightweight()
    }

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
            "schema_version":2,"tool":"reflect","session_id":"session","analysis_view":"overview",
            "topic":"overview","facet":"overview","depth":depth,"horizon":"session",
            "source_policy":"auto","include_context":false,"summary":text,
            "model_requests":{"coverage":"not_captured","records_observed":0,"accepted_records_observed":0},
            "data_coverage":{"overall":"fresh","source":"test","events":60,"decisions":0},
            "observations":observations,"evidence":evidence,
            "action_hints":[{"target_type":"tool","summary":text,"confidence":{},"observation_refs":["obs-59"]}],
            "graph_slice":{"nodes":nodes,"edges":[]}
        })).unwrap()
    }

    #[test]
    fn lightweight_judgment_groups_are_bounded_with_omission_count() {
        let mut report = large_report("hint");
        report.judgment_usage = Some(JudgmentUsageSummary {
            scope: super::super::JudgmentUsageScope::default(),
            capture_incomplete: false,
            coverage: "available".into(),
            groups: (0..10)
                .map(|index| JudgmentUsageGroup {
                    provider: "typesafe".into(),
                    offering_id: format!("offering-{index}"),
                    model: "jev".into(),
                    purpose: "request_judgment".into(),
                    operation: "request_judgment".into(),
                    attempts: 1,
                    exact_usage_attempts: 1,
                    known_input_tokens: 10,
                    known_output_tokens: 2,
                    input_observed: true,
                    output_observed: true,
                    input_incomplete: false,
                    output_incomplete: false,
                })
                .collect(),
            omitted_groups: 0,
        });
        report.model_requests = super::super::ModelRequestCapture {
            coverage: super::super::ModelRequestCaptureCoverage::WindowObserved,
            records_observed: 10,
            accepted_records_observed: 0,
            terminal: Some(super::super::ModelRequestSummary {
                groups: (0..10)
                    .map(|index| super::super::ModelRequestGroup {
                        run_id: Some(format!("run-{index}")),
                        parent_run_id: Some("parent-run".into()),
                        agent_id: Some("agent".into()),
                        offering_id: format!("offering-{index}"),
                        provider: "typesafe".into(),
                        model: "model".into(),
                        purpose: "primary_agent".into(),
                        terminal_requests: 1,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
        };
        let projected = report.project_lightweight();
        let usage = projected.judgment_usage.unwrap();
        assert_eq!(usage.groups.len(), 2);
        assert_eq!(usage.omitted_groups, 8);
        let model = projected.model_requests.terminal.unwrap();
        assert_eq!(model.groups.len(), 2);
        assert_eq!(model.omitted_groups, 8);
    }

    #[test]
    fn model_group_limit_marks_an_otherwise_untruncated_report() {
        let mut report = large_report("hint");
        report.summary = "small report".into();
        report.observations.clear();
        report.evidence.clear();
        report.action_hints.clear();
        report.failure_clusters.clear();
        report.graph_slice = Default::default();
        report.data_coverage.events = 0;
        report.model_requests = super::super::ModelRequestCapture {
            coverage: super::super::ModelRequestCaptureCoverage::WindowObserved,
            records_observed: 3,
            accepted_records_observed: 0,
            terminal: Some(super::super::ModelRequestSummary {
                groups: (0..3)
                    .map(|index| super::super::ModelRequestGroup {
                        run_id: Some(format!("run-{index}")),
                        offering_id: format!("offering-{index}"),
                        provider: "provider".into(),
                        model: "model".into(),
                        purpose: "agent_turn".into(),
                        terminal_requests: 1,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
        };

        let projected = report.project_lightweight();
        let terminal = projected.model_requests.terminal.as_ref().unwrap();
        assert_eq!(terminal.groups.len(), 2);
        assert_eq!(terminal.omitted_groups, 1);
        assert!(projected.budget_result.truncated);
    }

    #[test]
    fn model_request_groups_yield_before_execution_facts_at_report_budget() {
        use super::super::{
            EvidenceEvent, ModelRequestCaptureCoverage, ModelRequestGroup, ModelRequestSummary,
        };
        let event = EvidenceEvent {
            event_id: "dependency-fact".into(),
            event_type: "trace_span".into(),
            run_id: Some("parent-run".into()),
            metadata: Some(json!({"name":"agent_dependency_boundary", "attrs": {
                "parent_run_id":"parent-run", "outcome":"results_adopted",
                "children":serde_json::to_string(&json!([{"agent_id":"child", "run_id":"child-run", "status":"completed"}])).unwrap()
            }})),
            ..Default::default()
        };
        let mut report = project_events(&[event]);
        report.model_requests = super::super::ModelRequestCapture {
            coverage: ModelRequestCaptureCoverage::WindowObserved,
            records_observed: 8,
            accepted_records_observed: 0,
            terminal: Some(ModelRequestSummary {
                groups: (0..8)
                    .map(|index| ModelRequestGroup {
                        run_id: Some(format!("run-{index}")),
                        parent_run_id: Some("parent-run".into()),
                        agent_id: Some("child".into()),
                        offering_id: format!("offering-{index}"),
                        provider: "provider".into(),
                        model: "m".repeat(6_000),
                        purpose: "sub_agent".into(),
                        terminal_requests: 1,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
        };
        let projected = report.project_lightweight();
        let spine = &projected.graph_slice.nodes[0].metadata.as_ref().unwrap()["execution_spine"];
        assert_eq!(spine["facts"].as_array().unwrap().len(), 1);
        let terminal = projected.model_requests.terminal.as_ref().unwrap();
        assert!(terminal.omitted_groups > 0);
        assert!(terminal.groups.len() < 8);
        assert!(projected.budget_result.truncated);
        assert!(serde_json::to_vec(&projected).unwrap().len() < 20_000);
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
    fn optional_local_usage_does_not_replace_critical_summary_or_diagnosis() {
        for depth in ["hint", "summary"] {
            let mut report = large_report(depth);
            report.summary = "Execution failed: permission denied; do not retry mutation.".into();
            let expected = report.clone().project_lightweight();
            report.judgment_usage = Some(JudgmentUsageSummary {
                scope: super::super::JudgmentUsageScope::LocalCaptureUnavailable,
                capture_incomplete: true,
                coverage: "unavailable".into(),
                groups: vec![],
                omitted_groups: 0,
            });
            let projected = report.project_lightweight();
            assert_eq!(projected.summary, expected.summary);
            assert_eq!(projected.observations, expected.observations);
            assert_eq!(projected.action_hints, expected.action_hints);
            assert_eq!(projected.clone().project_lightweight(), projected);
        }
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

    #[test]
    fn retained_run_support_shares_the_serialized_budget() {
        let mut report = large_report("summary");
        for node in report.graph_slice.nodes.iter_mut().take(4) {
            node.metadata = Some(json!({"execution_spine": {
                "coverage":"partial", "task_completion":"not_established",
                "omitted_facts":0, "facts":[{"event_id":"a".repeat(3500)}]
            }}));
        }
        let projected = report.project_lightweight();
        assert!(
            serde_json::to_vec(&projected.graph_slice.nodes)
                .unwrap()
                .len()
                <= 8192
        );
        assert!(serde_json::to_vec(&projected).unwrap().len() < 20_000);
        assert!(projected.graph_slice.nodes.iter().any(|node| {
            node.metadata.as_ref().unwrap()["execution_spine"]["omitted_facts"]
                .as_u64()
                .unwrap()
                > 0
        }));
        assert_eq!(projected.clone().project_lightweight(), projected);
    }
}

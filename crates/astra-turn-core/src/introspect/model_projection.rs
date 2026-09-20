//! Bounded model view; the durable report and its machine-consumer schema stay intact.

use serde_json::{Value, json};

use super::{IntrospectReport, observation::observation_priority_key};

impl IntrospectReport {
    /// Fit complete semantic units, never fragments of JSON or evidence identities.
    /// The caller supplies the product model budget, not a user pagination limit.
    pub(crate) fn model_projection(&self, max_chars: usize) -> String {
        let mut projected = json!({
            "schema": "astra-introspect-model-projection-v1",
            "tool": "introspect",
            "snapshot_boundary": "before_current_introspect_execution",
            "recovery": "Inspect further only for needed evidence. Another introspect call creates a new snapshot, not the remainder of this one.",
            "observations": [],
            "evidence": [],
            "action_hints": [],
            "projection_budget": {
                "truncated": true,
                "omitted_fields": ["summary", "scope", "data_coverage", "runtime_feedback", "source_budget"],
                "summary_shortened": false
            }
        });
        if self.semantic_judgments.is_some() {
            projected["projection_budget"]["omitted_fields"]
                .as_array_mut()
                .unwrap()
                .push(json!("semantic_judgments"));
        }
        if self.tool_result_judgments.is_some() {
            projected["projection_budget"]["omitted_fields"]
                .as_array_mut()
                .unwrap()
                .push(json!("tool_result_judgments"));
        }
        if self.judgment_usage.is_some() {
            projected["projection_budget"]["omitted_fields"]
                .as_array_mut()
                .unwrap()
                .push(json!("judgment_usage"));
        }
        self.update_projection_counts(&mut projected);
        assert!(
            fits(&projected, max_chars),
            "introspection projection scaffold must fit the product budget"
        );

        let summary = if self.summary.chars().count() > 360 {
            format!("{}…", self.summary.chars().take(360).collect::<String>())
        } else {
            self.summary.clone()
        };
        let shortened = summary != self.summary;
        projected["projection_budget"]["summary_shortened"] = json!(shortened);
        self.fit_field(&mut projected, "summary", json!(summary), max_chars);
        self.fit_field(
            &mut projected,
            "scope",
            json!({
                "topic": self.topic, "facet": self.facet, "depth": self.depth,
                "horizon": self.horizon, "source_policy": self.source_policy
            }),
            max_chars,
        );
        self.fit_field(
            &mut projected,
            "source_budget",
            json!(self.budget_result),
            max_chars,
        );

        let mut observations = self.observations.iter().collect::<Vec<_>>();
        observations
            .sort_by_key(|observation| std::cmp::Reverse(observation_priority_key(observation)));
        // Give the most important fitting observation priority over the runtime frame.
        let mut frame_attempted = false;
        let mut usage_attempted = false;
        for observation in observations {
            if !usage_attempted
                && !matches!(
                    observation.severity.as_str(),
                    "critical" | "error" | "warning"
                )
            {
                self.fit_judgment_usage(&mut projected, max_chars);
                usage_attempted = true;
            }
            // Try one complete supporting evidence unit at a time. An oversized
            // first reference must not hide a critical fact supported elsewhere.
            let supports = if observation.evidence_refs.is_empty() {
                vec![None]
            } else {
                observation
                    .evidence_refs
                    .iter()
                    .filter_map(|reference| {
                        self.evidence
                            .iter()
                            .find(|evidence| &evidence.ref_id == reference)
                            .map(Some)
                    })
                    .collect()
            };
            for support in supports {
                let mut candidate = projected.clone();
                let mut retained = observation.clone();
                retained.evidence_refs = support
                    .iter()
                    .map(|evidence| evidence.ref_id.clone())
                    .collect();
                if let Some(evidence) = support
                    && !candidate["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["ref_id"] == evidence.ref_id)
                {
                    candidate["evidence"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!(evidence));
                }
                candidate["observations"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(retained));
                self.update_projection_counts(&mut candidate);
                if !fits(&candidate, max_chars) {
                    continue;
                }
                projected = candidate;
                if usage_attempted && !frame_attempted {
                    self.fit_field(
                        &mut projected,
                        "runtime_feedback",
                        json!(self.runtime_feedback),
                        max_chars,
                    );
                    frame_attempted = true;
                }
                break;
            }
        }
        if !usage_attempted {
            self.fit_judgment_usage(&mut projected, max_chars);
        }
        if !frame_attempted {
            self.fit_field(
                &mut projected,
                "runtime_feedback",
                json!(self.runtime_feedback),
                max_chars,
            );
        }
        self.fit_field(
            &mut projected,
            "data_coverage",
            json!(self.data_coverage),
            max_chars,
        );
        if let Some(semantics) = &self.semantic_judgments {
            self.fit_field(
                &mut projected,
                "semantic_judgments",
                json!(semantics),
                max_chars,
            );
        }
        if let Some(judgments) = &self.tool_result_judgments {
            self.fit_field(
                &mut projected,
                "tool_result_judgments",
                json!(judgments),
                max_chars,
            );
        }
        for hint in &self.action_hints {
            if hint.observation_refs.is_empty()
                || !hint.observation_refs.iter().all(|reference| {
                    projected["observations"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["ref_id"] == *reference)
                })
            {
                continue;
            }
            let mut candidate = projected.clone();
            candidate["action_hints"]
                .as_array_mut()
                .unwrap()
                .push(json!(hint));
            self.update_projection_counts(&mut candidate);
            if fits(&candidate, max_chars) {
                projected = candidate;
            }
        }
        projected.to_string()
    }

    fn fit_field(&self, projected: &mut Value, name: &str, value: Value, max_chars: usize) {
        let mut candidate = projected.clone();
        candidate[name] = value;
        candidate["projection_budget"]["omitted_fields"]
            .as_array_mut()
            .unwrap()
            .retain(|field| field != name);
        if fits(&candidate, max_chars) {
            *projected = candidate;
        }
    }

    fn fit_judgment_usage(&self, projected: &mut Value, max_chars: usize) {
        let Some(usage) = &self.judgment_usage else {
            return;
        };
        // Reuse ledger totals, including their unknown/partial coverage. The
        // model budget removes detail; it must never recompute aggregate usage
        // from whichever attempts or groups happen to fit.
        let mut compact = usage.clone();
        compact.omitted_attempts = compact
            .omitted_attempts
            .saturating_add(compact.attempts.len());
        compact.attempts.clear();
        compact.omitted_groups = compact.omitted_groups.saturating_add(compact.groups.len());
        compact.groups.clear();
        self.fit_field(projected, "judgment_usage", json!(compact), max_chars);
        if projected.get("judgment_usage").is_none() {
            return;
        }
        for group in &usage.groups {
            let mut candidate = compact.clone();
            candidate.groups.push(group.clone());
            candidate.omitted_groups = candidate.omitted_groups.saturating_sub(1);
            let mut with_group = projected.clone();
            with_group["judgment_usage"] = json!(candidate);
            if fits(&with_group, max_chars) {
                compact = candidate;
                *projected = with_group;
            }
        }
    }

    fn update_projection_counts(&self, projected: &mut Value) {
        let observations = projected["observations"].as_array().unwrap().len();
        let evidence = projected["evidence"].as_array().unwrap().len();
        let hints = projected["action_hints"].as_array().unwrap().len();
        projected["projection_budget"]["omitted"] = json!({
            "observations": self.observations.len().saturating_sub(observations),
            "evidence": self.evidence.len().saturating_sub(evidence),
            "action_hints": self.action_hints.len().saturating_sub(hints),
            "graph_nodes": self.graph_slice.nodes.len(),
            "graph_edges": self.graph_slice.edges.len(),
            "failure_clusters": self.failure_clusters.len()
        });
    }
}

fn fits(value: &Value, max_chars: usize) -> bool {
    value.to_string().chars().count() <= max_chars
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::introspect::{IntrospectRequest, IntrospectSnapshot, build_introspect_report};
    use crate::tool::result::sanitize::INTROSPECT_MODEL_RESULT_CHARS;

    #[test]
    fn ledger_summary_survives_oversized_evidence_without_recounting_groups() {
        use crate::introspect::{JudgmentUsageCoverage, JudgmentUsageGroup, JudgmentUsageSnapshot};
        let mut report = build_introspect_report(
            &IntrospectSnapshot::default(),
            &IntrospectRequest::default(),
        );
        for evidence in &mut report.evidence {
            evidence.summary = "large evidence ".repeat(2_000);
        }
        report.judgment_usage = Some(JudgmentUsageSnapshot {
            coverage: JudgmentUsageCoverage::Available,
            observed_attempts: Some(3),
            known_input_tokens: Some(1234),
            known_output_tokens: Some(0),
            input_complete: false,
            output_complete: true,
            omitted_attempts: 3,
            groups: vec![JudgmentUsageGroup {
                provider: "typesafe".into(),
                offering_id: "offering".into(),
                model: "jev".into(),
                operation: "tool_result_rerank".into(),
                attempts: 3,
                known_input_tokens: 1234,
                known_output_tokens: 0,
                input_complete: false,
                output_complete: true,
            }],
            ..Default::default()
        });
        let original = serde_json::to_value(&report).unwrap();
        let text = report.model_projection(INTROSPECT_MODEL_RESULT_CHARS);
        let projection: Value = serde_json::from_str(&text).unwrap();
        let usage = &projection["judgment_usage"];
        assert_eq!(usage["known_input_tokens"], 1234);
        assert_eq!(usage["known_output_tokens"], 0);
        assert_eq!(usage["input_complete"], false);
        assert_eq!(usage["output_complete"], true);
        assert_eq!(usage["observed_attempts"], 3);
        assert_eq!(usage["groups"][0]["operation"], "tool_result_rerank");
        assert!(
            !projection["projection_budget"]["omitted_fields"]
                .as_array()
                .unwrap()
                .contains(&json!("judgment_usage"))
        );
        assert!(text.chars().count() <= INTROSPECT_MODEL_RESULT_CHARS);
        assert_eq!(serde_json::to_value(&report).unwrap(), original);
    }

    #[test]
    fn unavailable_usage_and_display_omission_remain_distinct() {
        use crate::introspect::{JudgmentUsageCoverage, JudgmentUsageSnapshot};
        let mut report = build_introspect_report(
            &IntrospectSnapshot::default(),
            &IntrospectRequest::default(),
        );
        report.judgment_usage = Some(JudgmentUsageSnapshot::unavailable(
            JudgmentUsageCoverage::SourceExcluded,
        ));
        let full: Value =
            serde_json::from_str(&report.model_projection(INTROSPECT_MODEL_RESULT_CHARS)).unwrap();
        assert_eq!(full["judgment_usage"]["coverage"], "source_excluded");
        assert!(full["judgment_usage"]["known_input_tokens"].is_null());
        let mut narrow = json!({"projection_budget":{"omitted_fields":["judgment_usage"]}});
        report.fit_judgment_usage(&mut narrow, 100);
        assert!(narrow.get("judgment_usage").is_none());
        assert_eq!(
            narrow["projection_budget"]["omitted_fields"],
            json!(["judgment_usage"])
        );
    }

    #[test]
    fn warning_and_auxiliary_totals_precede_large_runtime_frame() {
        use crate::introspect::{JudgmentUsageCoverage, JudgmentUsageSnapshot};
        let mut report = build_introspect_report(
            &IntrospectSnapshot::default(),
            &IntrospectRequest::default(),
        );
        report.observations.truncate(1);
        report.observations[0].severity = "warning".into();
        report.observations[0].summary = "Provider evidence is partial".into();
        report.observations[0].evidence_refs.clear();
        report.runtime_feedback = Some(crate::introspect::test_runtime_feedback(3, 3, 0));
        report.judgment_usage = Some(JudgmentUsageSnapshot {
            coverage: JudgmentUsageCoverage::Available,
            observed_attempts: Some(1),
            known_input_tokens: Some(321),
            known_output_tokens: Some(17),
            ..Default::default()
        });
        let result: Value = serde_json::from_str(&report.model_projection(2400)).unwrap();
        assert_eq!(result["observations"][0]["severity"], "warning");
        assert_eq!(result["judgment_usage"]["known_input_tokens"], 321);
        assert_eq!(result["judgment_usage"]["known_output_tokens"], 17);
    }

    #[test]
    fn projection_prioritizes_critical_evidence_and_preserves_reference_closure() {
        let mut report = build_introspect_report(
            &IntrospectSnapshot::default(),
            &IntrospectRequest::default(),
        );
        let first = report.observations[0].clone();
        report.observations = (0..60)
            .map(|index| {
                let mut item = first.clone();
                item.ref_id = format!("urn:observation:{index}");
                item.summary = "routine detail ".repeat(30);
                item
            })
            .collect();
        let mut critical = first;
        critical.ref_id = "urn:observation:critical".into();
        critical.severity = "critical".into();
        critical.summary = "Executor outcome is unknown".into();
        let mut oversized_support = report.evidence[0].clone();
        oversized_support.ref_id = "indivisible-evidence-identity".repeat(1000);
        critical
            .evidence_refs
            .insert(0, oversized_support.ref_id.clone());
        report.evidence.insert(0, oversized_support);
        report.observations.push(critical);
        let before = serde_json::to_value(&report).unwrap();
        let text = report.model_projection(INTROSPECT_MODEL_RESULT_CHARS);
        let result: Value = serde_json::from_str(&text).unwrap();
        assert!(text.chars().count() <= INTROSPECT_MODEL_RESULT_CHARS);
        assert_eq!(
            result["observations"][0]["ref_id"],
            "urn:observation:critical"
        );
        for item in result["observations"].as_array().unwrap() {
            for reference in item["evidence_refs"].as_array().unwrap() {
                assert!(
                    result["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|e| &e["ref_id"] == reference)
                );
            }
        }
        assert_eq!(
            result["projection_budget"]["omitted"]["observations"]
                .as_u64()
                .unwrap() as usize,
            report.observations.len() - result["observations"].as_array().unwrap().len()
        );
        assert_eq!(serde_json::to_value(&report).unwrap(), before);
    }

    #[test]
    fn semantic_judgment_projection_only_reports_existing_omitted_field() {
        let mut report = build_introspect_report(
            &IntrospectSnapshot::default(),
            &IntrospectRequest::default(),
        );
        report.semantic_judgments = None;
        let projected: Value =
            serde_json::from_str(&report.model_projection(INTROSPECT_MODEL_RESULT_CHARS)).unwrap();
        assert!(
            !projected["projection_budget"]["omitted_fields"]
                .as_array()
                .unwrap()
                .contains(&json!("semantic_judgments"))
        );
        let view = astra_services::semantic_judgment_observation::SemanticJudgmentView {
            capture_gaps: vec![astra_services::semantic_judgment_observation::SemanticJudgmentCaptureGap::TraceMayBeDropped; INTROSPECT_MODEL_RESULT_CHARS],
            ..Default::default()
        };
        report.semantic_judgments = Some(view);
        let projected: Value =
            serde_json::from_str(&report.model_projection(INTROSPECT_MODEL_RESULT_CHARS)).unwrap();
        assert!(projected.get("semantic_judgments").is_none());
        assert!(
            projected["projection_budget"]["omitted_fields"]
                .as_array()
                .unwrap()
                .contains(&json!("semantic_judgments"))
        );
    }

    #[test]
    fn projection_omits_oversized_identity_and_missing_support_without_fabrication() {
        let mut report = build_introspect_report(
            &IntrospectSnapshot::default(),
            &IntrospectRequest::default(),
        );
        report.topic = "scope".repeat(2000);
        report.summary = "反省🙂".repeat(2000);
        report.observations[0].ref_id = "identity".repeat(2000);
        let mut missing = report.observations[0].clone();
        missing.ref_id = "urn:missing-support".into();
        missing.evidence_refs = vec!["urn:unavailable-evidence".into()];
        report.observations.push(missing);
        let text = report.model_projection(INTROSPECT_MODEL_RESULT_CHARS);
        let result: Value = serde_json::from_str(&text).unwrap();
        assert!(text.chars().count() <= INTROSPECT_MODEL_RESULT_CHARS);
        assert!(result["observations"].as_array().unwrap().is_empty());
        assert!(result["evidence"].as_array().unwrap().is_empty());
        assert!(result["scope"].is_null());
        assert!(
            result["projection_budget"]["omitted_fields"]
                .as_array()
                .unwrap()
                .contains(&json!("scope"))
        );
        assert_eq!(result["projection_budget"]["summary_shortened"], true);
    }
}

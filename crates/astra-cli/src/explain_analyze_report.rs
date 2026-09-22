//! Plain-text Explain Analyze rendering over canonical execution facts.
//!
//! This renderer consumes only typed lifecycle facts. It never reconstructs
//! work from trace prose, CLI wall-clock timers, or user/provider payloads.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::explain_analyze_format::{alpha_label, diagnostic_label, format_ms, format_tokens};
use astra_turn_types::{
    ExplainAnalyzeEventV1, ExplainAnalyzeGraphV1, ExplainAnalyzeNodeKindV1,
    ExplainAnalyzeOutcomeV1, ExplainAnalyzeUsageBasisV1,
};

const MAX_DIAGNOSTICS: usize = 5;

pub(crate) fn render(
    events: &[ExplainAnalyzeEventV1],
    verbose: bool,
    delivery_degraded: bool,
) -> String {
    if events.is_empty() {
        return if delivery_degraded {
            "Explain Analyze · incomplete · stream delivery was interrupted before runtime facts could be recovered.".to_string()
        } else {
            "Explain Analyze · no runtime facts were captured for this turn.".to_string()
        };
    }

    let mut graph = ExplainAnalyzeGraphV1::default();
    for event in events {
        graph.apply(event.clone());
    }
    graph.finish_ingest();

    let clocks = clock_labels(&graph);
    let measured = graph
        .nodes()
        .iter()
        .filter(|node| node.terminal_observed && node.duration_ms.is_some())
        .count();
    let all_terminal = graph.nodes().iter().all(|node| node.terminal_observed);
    let coverage_gaps = graph.coverage_gaps();
    let observation_incomplete = delivery_degraded || !coverage_gaps.is_empty();
    let status = if !delivery_degraded && graph.diagnostics().is_empty() && all_terminal {
        if coverage_gaps.is_empty() {
            "recorded"
        } else {
            "partial capture"
        }
    } else {
        "incomplete"
    };
    let overlap = graph
        .max_concurrency()
        .map(|count| {
            if !observation_incomplete {
                format!("{count} concurrent leaf stages")
            } else {
                format!("at least {count} overlapping recorded spans")
            }
        })
        .unwrap_or_else(|| "unavailable (timing coverage is incomplete)".to_string());

    let mut lines = vec![format!(
        "Explain Analyze · {status} · {} stages · {}/{} timed spans · {} clock domains",
        graph.nodes().len(),
        measured,
        graph.nodes().len(),
        clocks.len(),
    )];
    if verbose || graph.max_concurrency().is_none_or(|count| count > 1) {
        lines.push(format!("  Observed overlap · {overlap}"));
    }

    if delivery_degraded {
        lines.push(
            "  Observation gap · stream delivery was interrupted; later spans may be missing."
                .to_string(),
        );
    }

    append_tree(&graph, &clocks, &mut lines, verbose);

    if !coverage_gaps.is_empty() {
        let labels = coverage_gaps
            .iter()
            .take(if verbose { usize::MAX } else { 2 })
            .map(|gap| gap.label())
            .collect::<Vec<_>>()
            .join(" · ");
        let more = if !verbose && coverage_gaps.len() > 2 {
            format!(" · {} more in report", coverage_gaps.len() - 2)
        } else {
            String::new()
        };
        lines.push(format!("  Not timed separately · {labels}{more}"));
    }

    if let Some(summary) = provider_usage_summary(&graph) {
        lines.push(format!("  {summary}"));
    }

    lines.extend(
        auxiliary_usage_lines(&graph)
            .into_iter()
            .map(|line| format!("  {line}")),
    );
    lines.extend(
        auxiliary_details_lines(&graph)
            .into_iter()
            .map(|line| format!("  {line}")),
    );
    append_diagnostics(&graph, &mut lines);
    lines.join("\n")
}

fn append_tree(
    graph: &ExplainAnalyzeGraphV1,
    clocks: &HashMap<String, String>,
    lines: &mut Vec<String>,
    verbose: bool,
) {
    let mut seen = HashSet::new();
    let mut roots = graph.roots().collect::<Vec<_>>();
    roots.sort_unstable();
    let mut stack = Vec::new();
    let mut root_cursor = 0;
    let mut orphan_heading_written = false;

    loop {
        if stack.is_empty() {
            while root_cursor < roots.len() && seen.contains(&roots[root_cursor]) {
                root_cursor += 1;
            }
            if root_cursor < roots.len() {
                let root = roots[root_cursor];
                let last = root_cursor + 1 == roots.len();
                root_cursor += 1;
                stack.push((root, Vec::<bool>::new(), last));
            } else if let Some(orphan) =
                (0..graph.nodes().len()).find(|index| !seen.contains(index))
            {
                if !orphan_heading_written {
                    lines.push("Unlinked stages".to_string());
                    orphan_heading_written = true;
                }
                stack.push((orphan, Vec::new(), true));
            } else {
                break;
            }
        }

        while let Some((index, ancestor_has_sibling, last)) = stack.pop() {
            if !seen.insert(index) {
                continue;
            }
            let Some(node) = graph.nodes().get(index) else {
                continue;
            };
            let tree = tree_prefix(&ancestor_has_sibling, last);
            let clock = clocks
                .get(&node.clock_domain_id)
                .map(String::as_str)
                .unwrap_or("clock ?");
            let offset = format!("{clock} +{}", format_ms(node.start_elapsed_ms));
            let duration = node
                .duration_ms
                .map(format_ms)
                .unwrap_or_else(|| "not measured".to_string());
            let state = node_state(node.terminal_observed, node.outcome);
            let round = node
                .round_index
                .filter(|round| verbose || *round > 0)
                .map(|round| format!(" · round {}", u64::from(round).saturating_add(1)))
                .unwrap_or_default();
            let attempt = node
                .attempt_index
                .filter(|attempt| verbose || *attempt > 0)
                .map(|attempt| format!(" · attempt {}", u64::from(attempt).saturating_add(1)))
                .unwrap_or_default();
            lines.push(format!(
                "{tree}{} · {duration} · {state}{round}{attempt} · {offset}",
                safe_label(&node.label),
            ));

            if let Some(usage) = &node.usage {
                lines.push(format!(
                    "{}{}",
                    detail_prefix(&ancestor_has_sibling, last),
                    format_usage(
                        usage.basis,
                        usage.fresh_input_tokens,
                        usage.cache_read_tokens,
                        usage.cache_creation_tokens,
                        usage.output_tokens
                    ),
                ));
            }
            for report in node
                .context
                .as_ref()
                .and_then(|c| c.assembly.as_ref())
                .into_iter()
                .flat_map(|a| &a.edge_memory_selection)
            {
                let prefix = detail_prefix(&ancestor_has_sibling, last);
                lines.push(format!("{prefix}{}", report.summary()));
                if verbose {
                    lines.extend(
                        report
                            .detail_lines()
                            .into_iter()
                            .map(|line| format!("{prefix}  {line}")),
                    );
                }
            }
            if verbose && let Some(context) = &node.context {
                if let Some(budget) = &context.budget {
                    let prefix = detail_prefix(&ancestor_has_sibling, last);
                    lines.push(format!(
                        "{prefix}Request budget (estimate) · {} input / {} limit",
                        format_tokens(budget.estimated_input_tokens),
                        format_tokens(budget.effective_input_limit_tokens)
                    ));
                    lines.push(format!(
                        "{prefix}  System {} · tools {} · {} visible tools",
                        format_tokens(budget.estimated_system_tokens),
                        format_tokens(budget.tool_schema_tokens),
                        budget.visible_tool_count
                    ));
                    lines.push(format!(
                        "{prefix}  Output reserved {} · protocol {} · model context {}",
                        format_tokens(budget.requested_output_tokens),
                        format_tokens(budget.reserved_protocol_tokens),
                        format_tokens(budget.model_context_limit_tokens)
                    ));
                }
                if let Some(assembly) = &context.assembly {
                    for source in &assembly.sources {
                        lines.push(format!(
                            "{}{} · {} tokens · {} sections (runtime estimate)",
                            detail_prefix(&ancestor_has_sibling, last),
                            source_label(source.kind),
                            format_tokens(source.estimated_tokens),
                            source.section_count,
                        ));
                    }
                }
            }
            if verbose && !node.dependency_node_ids.is_empty() {
                let dependencies = node
                    .dependency_indices
                    .iter()
                    .map(|index| {
                        index
                            .and_then(|index| graph.nodes().get(index))
                            .map(|dependency| safe_label(&dependency.label))
                            .unwrap_or_else(|| "unrecorded stage".to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                lines.push(format!(
                    "{}Dependencies · {}",
                    detail_prefix(&ancestor_has_sibling, last),
                    dependencies
                ));
            }

            let mut children = graph.children(index).to_vec();
            if children
                .iter()
                .all(|child| graph.nodes()[*child].clock_domain_id == node.clock_domain_id)
            {
                children.sort_by_key(|child| graph.nodes()[*child].start_elapsed_ms);
            }
            let mut child_ancestors = ancestor_has_sibling;
            child_ancestors.push(!last);
            for (position, child) in children.iter().enumerate().rev() {
                stack.push((
                    *child,
                    child_ancestors.clone(),
                    position + 1 == children.len(),
                ));
            }
        }
    }
}

fn append_diagnostics(graph: &ExplainAnalyzeGraphV1, lines: &mut Vec<String>) {
    if graph.diagnostics().is_empty() {
        return;
    }
    let labels = graph
        .diagnostics()
        .iter()
        .map(|diagnostic| diagnostic_label(diagnostic.code))
        .collect::<BTreeSet<_>>();
    let details = labels
        .into_iter()
        .take(MAX_DIAGNOSTICS)
        .collect::<Vec<_>>()
        .join(" · ");
    let more = graph.diagnostics().len().saturating_sub(MAX_DIAGNOSTICS);
    if more == 0 {
        lines.push(format!("Observation gaps · {details}"));
    } else {
        lines.push(format!("Observation gaps · {details} · {more} more"));
    }
}

fn provider_usage_summary(graph: &ExplainAnalyzeGraphV1) -> Option<String> {
    let attempts = graph
        .nodes()
        .iter()
        .filter(|node| node.kind == ExplainAnalyzeNodeKindV1::ProviderAttempt)
        .collect::<Vec<_>>();
    if attempts.is_empty() {
        return None;
    }
    let reported = attempts.iter().filter(|node| node.usage.is_some()).count();
    Some(format!(
        "Provider usage · {reported}/{} requests reported tokens; cache counts are shown separately",
        attempts.len(),
    ))
}

fn format_usage(
    basis: ExplainAnalyzeUsageBasisV1,
    fresh: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    output: Option<u64>,
) -> String {
    let basis = match basis {
        ExplainAnalyzeUsageBasisV1::ProviderExact => "Provider reported",
        ExplainAnalyzeUsageBasisV1::ProviderPartial => "Partial provider report",
        ExplainAnalyzeUsageBasisV1::RuntimeEstimated => "Runtime estimate",
    };
    let mut lanes = Vec::new();
    if let Some(value) = fresh {
        lanes.push(format!("input {}", format_tokens(value)));
    }
    if let Some(value) = cache_read {
        lanes.push(format!("cache read {}", format_tokens(value)));
    }
    if let Some(value) = cache_creation {
        lanes.push(format!("cache write {}", format_tokens(value)));
    }
    if let Some(value) = output {
        lanes.push(format!("output {}", format_tokens(value)));
    }
    format!("{basis} tokens · {}", lanes.join(" · "))
}

fn clock_labels(graph: &ExplainAnalyzeGraphV1) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    for node in graph.nodes() {
        if !labels.contains_key(&node.clock_domain_id) {
            let ordinal = labels.len();
            labels.insert(
                node.clock_domain_id.clone(),
                format!("clock {}", alpha_label(ordinal)),
            );
        }
    }
    labels
}

fn tree_prefix(ancestors: &[bool], last: bool) -> String {
    let mut prefix = String::from("  ");
    for has_sibling in ancestors {
        prefix.push_str(if *has_sibling { "│  " } else { "   " });
    }
    prefix.push_str(if last { "└─ " } else { "├─ " });
    prefix
}

fn detail_prefix(ancestors: &[bool], last: bool) -> String {
    let mut prefix = String::from("  ");
    for has_sibling in ancestors {
        prefix.push_str(if *has_sibling { "│  " } else { "   " });
    }
    prefix.push_str(if last { "   " } else { "│  " });
    prefix.push_str("   ");
    prefix
}

fn node_state(terminal: bool, outcome: Option<ExplainAnalyzeOutcomeV1>) -> &'static str {
    if !terminal {
        return "still running or terminal fact missing";
    }
    match outcome {
        Some(
            ExplainAnalyzeOutcomeV1::Completed
            | ExplainAnalyzeOutcomeV1::Succeeded
            | ExplainAnalyzeOutcomeV1::Resolved,
        ) => "completed",
        Some(ExplainAnalyzeOutcomeV1::Failed | ExplainAnalyzeOutcomeV1::Rejected) => "failed",
        Some(ExplainAnalyzeOutcomeV1::Cancelled) => "cancelled",
        Some(ExplainAnalyzeOutcomeV1::Interrupted) => "interrupted",
        Some(
            ExplainAnalyzeOutcomeV1::Blocked
            | ExplainAnalyzeOutcomeV1::Waiting
            | ExplainAnalyzeOutcomeV1::Deferred,
        ) => "waiting",
        Some(ExplainAnalyzeOutcomeV1::Reused) => "reused",
        Some(ExplainAnalyzeOutcomeV1::Suppressed) => "skipped",
        Some(ExplainAnalyzeOutcomeV1::Fallback) => "fallback",
        Some(ExplainAnalyzeOutcomeV1::Unavailable) => "unavailable",
        Some(ExplainAnalyzeOutcomeV1::Delegated) => "delegated",
        None => "outcome not recorded",
    }
}

fn safe_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect()
}

fn source_label(kind: astra_turn_types::ExplainAnalyzeContextSourceKindV1) -> &'static str {
    use astra_turn_types::ExplainAnalyzeContextSourceKindV1::*;
    match kind {
        Identity => "Identity context",
        SelfModel => "Self model",
        ProjectContext => "Project context",
        DeferredTools => "Deferred tools",
        AvailableSkills => "Available skills",
        Memory => "Memory",
        WorkingMemory => "Working memory",
        History => "Conversation history",
        Constraints => "Constraints",
        Skills => "Skills",
        RuntimeIdentity => "Runtime identity",
        RuntimeVolatile => "Runtime state",
        EmergentSkills => "Emergent skills",
        EmergentMemory => "Emergent memory",
        EmergentSummary => "Emergent summary",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        EXPLAIN_ANALYZE_SCHEMA_VERSION, ExplainAnalyzeContextAssemblyBasisV1,
        ExplainAnalyzeContextAssemblyV1, ExplainAnalyzeContextBudgetBasisV1,
        ExplainAnalyzeContextBudgetV1, ExplainAnalyzeContextMetricsV1,
        ExplainAnalyzeContextSourceKindV1, ExplainAnalyzeContextSourceV1,
        ExplainAnalyzeCoverageGapV1, ExplainAnalyzeTokenUsageV1, ExplainAnalyzeTransitionV1,
    };

    #[allow(clippy::too_many_arguments)]
    fn fact(
        id: &str,
        node: &str,
        parent: Option<&str>,
        kind: ExplainAnalyzeNodeKindV1,
        transition: ExplainAnalyzeTransitionV1,
        start: u64,
        duration: Option<u64>,
        outcome: Option<ExplainAnalyzeOutcomeV1>,
    ) -> ExplainAnalyzeEventV1 {
        ExplainAnalyzeEventV1 {
            auxiliary_usage: None,
            auxiliary_details: None,
            schema_version: EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: id.to_string(),
            run_id: "run-1".to_string(),
            turn_id: "turn-1".to_string(),
            node_id: node.to_string(),
            parent_node_id: parent.map(str::to_string),
            dependency_node_ids: Vec::new(),
            producer_id: "runtime".to_string(),
            clock_domain_id: "clock-1".to_string(),
            kind,
            round_index: (kind == ExplainAnalyzeNodeKindV1::ModelRound
                || kind == ExplainAnalyzeNodeKindV1::ProviderAttempt)
                .then_some(0),
            attempt_index: (kind == ExplainAnalyzeNodeKindV1::ProviderAttempt).then_some(0),
            label: match kind {
                ExplainAnalyzeNodeKindV1::Turn => "User turn",
                ExplainAnalyzeNodeKindV1::Preparation => "Prepare model request",
                ExplainAnalyzeNodeKindV1::ContextAssembly => "Assemble context",
                ExplainAnalyzeNodeKindV1::ModelRound => "Generate response",
                ExplainAnalyzeNodeKindV1::ProviderAttempt => "Model provider request",
                _ => "Execute work",
            }
            .to_string(),
            transition,
            elapsed_ms: start + duration.unwrap_or_default(),
            start_elapsed_ms: (transition == ExplainAnalyzeTransitionV1::Finished).then_some(start),
            duration_ms: duration,
            outcome,
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        }
    }

    fn finished(mut start: ExplainAnalyzeEventV1, duration: u64) -> ExplainAnalyzeEventV1 {
        start.event_id.push_str("-done");
        start.transition = ExplainAnalyzeTransitionV1::Finished;
        start.elapsed_ms = start.elapsed_ms.saturating_add(duration);
        start.start_elapsed_ms = Some(start.elapsed_ms.saturating_sub(duration));
        start.duration_ms = Some(duration);
        start.outcome = Some(ExplainAnalyzeOutcomeV1::Succeeded);
        start
    }

    #[test]
    fn auxiliary_jev_usage_is_separate_and_missing_lanes_remain_unknown() {
        use astra_turn_types::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1, ExplainAnalyzeUsageBasisV1,
        };
        let start = fact(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        let mut end = finished(start.clone(), 100);
        end.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
            available: true,
            truncated: false,
            attempts: vec![ExplainAnalyzeAuxiliaryAttemptV1 {
                attempt_id: "aux-1".into(),
                provider: "typesafe".into(),
                offering_id: "jev-1".into(),
                model_name: "jev1".into(),
                purpose: "memory_retrieval_rerank".into(),
                operation_id: "relevance".into(),
                usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderPartial,
                usage: Some(ExplainAnalyzeTokenUsageV1 {
                    basis: ExplainAnalyzeUsageBasisV1::ProviderPartial,
                    fresh_input_tokens: Some(42),
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_creation_tokens: None,
                }),
            }],
        }));
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(start);
        graph.apply(end);
        let output = auxiliary_usage_lines(&graph).join("\n");
        assert!(output.contains("Jev"), "{output}");
        assert!(output.contains("in 42"), "{output}");
        assert!(output.contains("out unknown"), "{output}");
        assert!(output.contains("partial"), "{output}");
        assert!(output.contains("offering jev-1"), "{output}");
        assert!(output.contains("operation relevance"), "{output}");
    }

    #[test]
    fn auxiliary_details_explain_logical_timing_and_admission_settlement() {
        use astra_turn_types::{
            ExplainAnalyzeAdmissionSettlementReasonV1, ExplainAnalyzeAdmissionSettlementV1,
            ExplainAnalyzeAuxiliaryCallV1, ExplainAnalyzeAuxiliaryDetailsV1,
            RequestJudgmentClassificationV1, RequestJudgmentMutationV1, RequestJudgmentResultV1,
            RequestJudgmentScopeV1,
        };
        let start = fact(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        let mut end = finished(start.clone(), 100);
        end.auxiliary_details = Some(Box::new(ExplainAnalyzeAuxiliaryDetailsV1 {
            calls: vec![ExplainAnalyzeAuxiliaryCallV1 {
                call_id: "request_judgment:initial:0".into(),
                operation_id: "request_judgment".into(),
                stage: "initial".into(),
                start_elapsed_ms: 7,
                duration_ms: 12,
                outcome: ExplainAnalyzeOutcomeV1::Succeeded,
            }],
            truncated: false,
            admission: Some(ExplainAnalyzeAdmissionSettlementV1 {
                status: astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::Accepted,
                reason: ExplainAnalyzeAdmissionSettlementReasonV1::Accepted,
                classification: Some(RequestJudgmentResultV1::Decided {
                    classification: RequestJudgmentClassificationV1 {
                        work_required: false,
                        activation_deferred: false,
                        domain: None,
                        mutation: RequestJudgmentMutationV1::ReadOnly,
                        scope: RequestJudgmentScopeV1::Unknown,
                        parallel_subruns: false,
                        capabilities: Vec::new(),
                    },
                }),
                decision: Some(RequestJudgmentResultV1::Decided {
                    classification: RequestJudgmentClassificationV1 {
                        work_required: false,
                        activation_deferred: false,
                        domain: None,
                        mutation: RequestJudgmentMutationV1::ReadOnly,
                        scope: RequestJudgmentScopeV1::Unknown,
                        parallel_subruns: false,
                        capabilities: Vec::new(),
                    },
                }),
            }),
        }));
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(start);
        graph.apply(end);
        let output = auxiliary_details_lines(&graph).join("\n");
        assert!(output.contains("Auxiliary scope · turn"), "{output}");
        assert!(
            output.contains("Auxiliary timing · Request classification"),
            "{output}"
        );
        assert!(
            output.contains("stage initial · 12ms · client outcome succeeded · starts +7ms"),
            "{output}"
        );
        assert!(
            output.contains("logical client interval; overlapping intervals are not added"),
            "{output}"
        );
        assert!(
            output.contains("Admission settlement · status accepted · reason"),
            "{output}"
        );
        assert!(output.contains("\"result\":\"decided\""), "{output}");
        assert!(output.contains("\"work_required\":false"), "{output}");
        assert!(output.contains("reconciled decision"), "{output}");

        let mut second_start = fact(
            "second-turn-start",
            "second-turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        second_start.turn_id = "turn-2".into();
        second_start.clock_domain_id = "clock-2".into();
        let mut second_end = finished(second_start, 80);
        second_end.auxiliary_details = Some(Box::new(ExplainAnalyzeAuxiliaryDetailsV1 {
            calls: vec![ExplainAnalyzeAuxiliaryCallV1 {
                call_id: "work_plan:initial:0".into(),
                operation_id: "work_plan".into(),
                stage: "initial".into(),
                start_elapsed_ms: 3,
                duration_ms: 9,
                outcome: ExplainAnalyzeOutcomeV1::Failed,
            }],
            truncated: false,
            admission: None,
        }));
        graph.apply(second_end);
        let output = auxiliary_details_lines(&graph).join("\n");
        assert!(output.contains("Auxiliary scope · second-turn"), "{output}");
        assert!(output.contains("work_plan:initial:0"), "{output}");
    }

    #[test]
    fn auxiliary_known_tokens_are_lower_bounds_when_a_peer_attempt_is_unreported() {
        use astra_turn_types::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1, ExplainAnalyzeUsageBasisV1,
        };
        let start = fact(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        let mut end = finished(start.clone(), 100);
        let exact = ExplainAnalyzeAuxiliaryAttemptV1 {
            attempt_id: "reported".into(),
            provider: "typesafe".into(),
            offering_id: "jev-1".into(),
            model_name: "jev1".into(),
            purpose: "introspection".into(),
            operation_id: "request_judgment".into(),
            usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
            usage: Some(ExplainAnalyzeTokenUsageV1 {
                basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(40),
                output_tokens: Some(5),
                cache_read_tokens: None,
                cache_creation_tokens: None,
            }),
        };
        end.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
            available: true,
            truncated: false,
            attempts: vec![
                exact.clone(),
                ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: "missing".into(),
                    usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1::Unavailable,
                    usage: None,
                    ..exact
                },
            ],
        }));
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(start);
        graph.apply(end);
        let output = auxiliary_usage_lines(&graph).join("\n");
        assert!(output.contains("Request classification"), "{output}");
        assert!(output.contains("in at least 40"), "{output}");
        assert!(output.contains("out at least 5"), "{output}");
        assert!(output.contains("1/2 requests reported"), "{output}");
    }

    #[test]
    fn auxiliary_capture_truncation_and_partial_turn_coverage_preserve_lower_bounds() {
        use astra_turn_types::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1, ExplainAnalyzeUsageBasisV1,
        };
        let attempt = ExplainAnalyzeAuxiliaryAttemptV1 {
            attempt_id: "attempt".into(),
            provider: "typesafe".into(),
            offering_id: "offering".into(),
            model_name: "model".into(),
            purpose: "introspection".into(),
            operation_id: "request_judgment".into(),
            usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
            usage: Some(ExplainAnalyzeTokenUsageV1 {
                basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(2),
                output_tokens: Some(3),
                cache_read_tokens: Some(0),
                cache_creation_tokens: Some(1),
            }),
        };
        let ledger: Vec<_> = (0..129)
            .map(|i| ExplainAnalyzeAuxiliaryAttemptV1 {
                attempt_id: format!("attempt-{i}"),
                ..attempt.clone()
            })
            .collect();
        let captured = ExplainAnalyzeAuxiliaryUsageV1 {
            available: true,
            truncated: ledger.len() > 128,
            attempts: ledger.into_iter().take(128).collect(),
        };
        let graph_for = |captures: Vec<ExplainAnalyzeAuxiliaryUsageV1>| {
            let mut graph = ExplainAnalyzeGraphV1::default();
            for (i, capture) in captures.into_iter().enumerate() {
                let start = fact(
                    &format!("start-{i}"),
                    &format!("turn-{i}"),
                    None,
                    ExplainAnalyzeNodeKindV1::Turn,
                    ExplainAnalyzeTransitionV1::Started,
                    0,
                    None,
                    None,
                );
                let mut end = finished(start.clone(), 100);
                end.auxiliary_usage = Some(Box::new(capture));
                graph.apply(start);
                graph.apply(end);
            }
            graph
        };
        let output = auxiliary_usage_lines(&graph_for(vec![captured])).join("\n");
        for expected in [
            "in at least 256",
            "out at least 384",
            "cache read at least 0",
            "cache write at least 128",
            "128/128 captured requests reported",
            "capture truncated",
        ] {
            assert!(output.contains(expected), "missing {expected}: {output}");
        }
        assert!(!output.contains("capture unavailable"));
        let complete = ExplainAnalyzeAuxiliaryUsageV1 {
            available: true,
            truncated: false,
            attempts: vec![attempt],
        };
        let unavailable = ExplainAnalyzeAuxiliaryUsageV1 {
            available: false,
            truncated: false,
            attempts: vec![],
        };
        let output = auxiliary_usage_lines(&graph_for(vec![complete, unavailable])).join("\n");
        assert!(output.contains("in at least 2"), "{output}");
        assert!(output.contains("out at least 3"));
        assert!(output.contains("1/1 captured requests reported"));
        assert!(output.contains("capture unavailable"));
        assert!(!output.contains("capture truncated"));
        let empty = ExplainAnalyzeAuxiliaryUsageV1 {
            available: true,
            truncated: true,
            attempts: vec![],
        };
        let output = auxiliary_usage_lines(&graph_for(vec![empty])).join("\n");
        assert!(output.contains("full usage unknown"));
        assert!(!output.contains("in 0"));
        assert!(!output.contains("0/0"));
    }

    #[test]
    fn conflicting_auxiliary_evidence_is_visible_without_token_totals() {
        use astra_turn_types::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1, ExplainAnalyzeTokenUsageV1,
        };
        let mut graph = ExplainAnalyzeGraphV1::default();
        let mut events = Vec::new();
        for (i, count) in [731, 947].into_iter().enumerate() {
            let start = fact(
                &format!("start-{i}"),
                &format!("turn-{i}"),
                None,
                ExplainAnalyzeNodeKindV1::Turn,
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            );
            let mut end = finished(start.clone(), 100);
            end.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: vec![ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: "same-physical-attempt".into(),
                    provider: "provider".into(),
                    offering_id: "offering".into(),
                    model_name: "model".into(),
                    purpose: "verification_judge".into(),
                    operation_id: "request_judgment".into(),
                    usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
                    usage: Some(ExplainAnalyzeTokenUsageV1 {
                        basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
                        fresh_input_tokens: Some(count),
                        output_tokens: Some(0),
                        cache_read_tokens: None,
                        cache_creation_tokens: None,
                    }),
                }],
            }));
            for event in [start, end] {
                graph.apply(event.clone());
                events.push(event);
            }
        }
        // TUI consumes the same section; its full renderer has a local test.
        for output in [
            auxiliary_usage_lines(&graph).join("\n"),
            render(&events, false, false),
            crate::explain_analyze_html::render(&events, false, false),
        ] {
            assert!(
                output.contains("conflicting physical attempt evidence (1 identities)"),
                "{output}"
            );
            assert!(output.contains("no token total inferred"));
            assert!(!output.contains("731") && !output.contains("947"));
            assert!(!output.contains("capture truncated"));
        }
        let mut no_usage = events[1].clone();
        no_usage.auxiliary_usage = None;
        for records in [
            vec![no_usage.clone(), events[1].clone(), events[3].clone()],
            vec![events[3].clone(), events[1].clone(), no_usage],
        ] {
            for output in [
                render(&records, false, false),
                crate::explain_analyze_html::render(&records, false, false),
            ] {
                assert!(output.contains("conflicting turn/usage facts"), "{output}");
                assert!(output.contains("no token total inferred"));
                assert!(!output.contains("731") && !output.contains("947"));
                assert!(!output.contains("capture truncated"));
            }
        }
    }

    #[test]
    fn auxiliary_usage_keeps_request_classification_skill_selection_and_work_planning_separate() {
        use astra_turn_types::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1,
        };
        let start = fact(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        let mut end = finished(start.clone(), 100);
        let operations = [
            ("request_judgment", "Request classification", 10),
            ("skill_auto_route", "Skill selection", 20),
            ("work_plan", "Work planning", 30),
        ];
        end.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
            available: true,
            truncated: false,
            attempts: operations
                .iter()
                .map(|(operation, _, tokens)| ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: format!("aux-{operation}"),
                    provider: "openai".into(),
                    offering_id: "same-offering".into(),
                    model_name: "same-model".into(),
                    purpose: "introspection".into(),
                    operation_id: (*operation).into(),
                    usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
                    usage: Some(ExplainAnalyzeTokenUsageV1 {
                        basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
                        fresh_input_tokens: Some(*tokens),
                        output_tokens: Some(1),
                        cache_read_tokens: None,
                        cache_creation_tokens: None,
                    }),
                })
                .collect(),
        }));
        let output = render(&[start, end], false, false);
        let lines = output
            .lines()
            .filter(|line| line.contains("Auxiliary tokens"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{output}");
        for (_, label, tokens) in operations {
            let line = lines.iter().find(|line| line.contains(label)).unwrap();
            assert!(line.contains(&format!("in {tokens} ·")), "{line}");
            assert!(line.contains("1/1 requests reported"), "{line}");
            assert!(line.contains("cache read unknown"), "{line}");
        }
        assert_eq!(
            auxiliary_usage_label("completion_proxy:verification_judge", "verification_judge"),
            "Verification"
        );
        assert_eq!(
            auxiliary_usage_label("completion_proxy:introspection", "introspection"),
            "Request analysis"
        );
        assert_eq!(
            auxiliary_usage_label("unrecognized", "introspection"),
            "Request analysis"
        );
        assert_eq!(
            auxiliary_usage_label("unrecognized", "unrecognized"),
            "Auxiliary inference"
        );
        assert_eq!(
            auxiliary_usage_label("request_judgment", "introspection"),
            "Request classification"
        );
    }

    #[test]
    fn mixed_jev_and_llm_usage_remains_isolated_across_repeated_capture_segments() {
        use astra_turn_types::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1,
        };
        let attempts = [
            (
                "jev-decision",
                "typesafe",
                "jev-model",
                "request_judgment",
                100,
                3,
                None,
                None,
            ),
            (
                "llm-decision",
                "openai",
                "llm-model",
                "request_judgment",
                40,
                5,
                Some(60),
                Some(0),
            ),
            (
                "llm-plan",
                "openai",
                "llm-model",
                "work_plan",
                200,
                20,
                Some(10),
                Some(7),
            ),
        ]
        .into_iter()
        .map(
            |(id, provider, model, operation, input, output, read, write)| {
                ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: id.into(),
                    provider: provider.into(),
                    offering_id: format!("offering-{provider}"),
                    model_name: model.into(),
                    purpose: "introspection".into(),
                    operation_id: operation.into(),
                    usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
                    usage: Some(ExplainAnalyzeTokenUsageV1 {
                        basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
                        fresh_input_tokens: Some(input),
                        output_tokens: Some(output),
                        cache_read_tokens: read,
                        cache_creation_tokens: write,
                    }),
                }
            },
        )
        .collect::<Vec<_>>();
        let mut events = Vec::new();
        for (segment, offset) in [("first", 0), ("second", 100)] {
            let start = fact(
                &format!("{segment}-start"),
                segment,
                None,
                ExplainAnalyzeNodeKindV1::Turn,
                ExplainAnalyzeTransitionV1::Started,
                offset,
                None,
                None,
            );
            let mut end = finished(start.clone(), 100);
            end.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: attempts.clone(),
            }));
            events.extend([start, end]);
        }
        let output = render(&events, false, false);
        let lines = output
            .lines()
            .filter(|line| line.contains("Auxiliary tokens"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{output}");
        for (identity, label, counts) in [
            (
                "Jev (jev-model)",
                "Request classification",
                "in 100 · cache read unknown · cache write unknown · out 3",
            ),
            (
                "openai (llm-model)",
                "Request classification",
                "in 40 · cache read 60 · cache write 0 · out 5",
            ),
            (
                "openai (llm-model)",
                "Work planning",
                "in 200 · cache read 10 · cache write 7 · out 20",
            ),
        ] {
            let line = lines
                .iter()
                .find(|line| line.contains(identity) && line.contains(label))
                .unwrap();
            assert!(line.contains(counts), "{line}");
            assert!(line.contains("1/1 requests reported"), "{line}");
            assert!(!line.contains("partial"), "{line}");
        }
    }

    #[test]
    fn renders_canonical_tree_timing_usage_and_context_without_trace_prose() {
        let mut turn_start = fact(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        turn_start.label = "User turn".to_string();
        let mut turn_end = finished(turn_start.clone(), 540);
        turn_end.outcome = Some(ExplainAnalyzeOutcomeV1::Completed);

        let prep_start = fact(
            "prep-start",
            "prep",
            Some("turn"),
            ExplainAnalyzeNodeKindV1::Preparation,
            ExplainAnalyzeTransitionV1::Started,
            2,
            None,
            None,
        );
        let mut prep_end = finished(prep_start.clone(), 80);
        prep_end.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: Some(ExplainAnalyzeContextBudgetV1 {
                basis: ExplainAnalyzeContextBudgetBasisV1::PreProviderEstimate,
                estimated_input_tokens: 830,
                estimated_system_tokens: 420,
                tool_schema_tokens: 160,
                requested_output_tokens: 900,
                reserved_protocol_tokens: 90,
                effective_input_limit_tokens: 8_000,
                model_context_limit_tokens: 16_000,
                visible_tool_count: 12,
            }),
            assembly: None,
        });

        let assembly_start = fact(
            "assembly-start",
            "assembly",
            Some("prep"),
            ExplainAnalyzeNodeKindV1::ContextAssembly,
            ExplainAnalyzeTransitionV1::Started,
            10,
            None,
            None,
        );
        let mut assembly_end = finished(assembly_start.clone(), 30);
        assembly_end.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: None,
            assembly: Some(Box::new(ExplainAnalyzeContextAssemblyV1 {
                edge_memory_selection: vec![
                    serde_json::from_value(serde_json::json!({
                        "session_id":"s", "turn":1, "operation":"relevance", "method":"model",
                        "reason":"completed", "model":"jev-test", "elapsed_ms":398,
                        "selection_order":[0], "candidates":[{"index":0,"selected":true,"probability_bps":9000},
                                      {"index":1,"selected":false,"probability_bps":1000}]
                    }))
                    .unwrap(),
                ],
                basis: ExplainAnalyzeContextAssemblyBasisV1::RuntimeTextEstimate,
                sources: vec![ExplainAnalyzeContextSourceV1 {
                    kind: ExplainAnalyzeContextSourceKindV1::Memory,
                    section_count: 3,
                    estimated_tokens: 90,
                }],
            })),
        });

        let model_start = fact(
            "model-start",
            "model",
            Some("turn"),
            ExplainAnalyzeNodeKindV1::ModelRound,
            ExplainAnalyzeTransitionV1::Started,
            90,
            None,
            None,
        );
        let attempt_start = fact(
            "attempt-start",
            "attempt",
            Some("model"),
            ExplainAnalyzeNodeKindV1::ProviderAttempt,
            ExplainAnalyzeTransitionV1::Started,
            100,
            None,
            None,
        );
        let mut attempt_end = finished(attempt_start.clone(), 220);
        attempt_end.usage = Some(ExplainAnalyzeTokenUsageV1 {
            basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
            fresh_input_tokens: Some(800),
            cache_read_tokens: Some(40),
            cache_creation_tokens: Some(10),
            output_tokens: Some(55),
        });
        let model_end = finished(model_start.clone(), 250);

        let output = render(
            &[
                turn_start,
                prep_start,
                prep_end,
                assembly_start,
                assembly_end,
                model_start,
                attempt_start,
                attempt_end,
                model_end,
                turn_end,
            ],
            true,
            false,
        );
        assert!(
            output.contains(
                "Explain Analyze · recorded · 5 stages · 5/5 timed spans · 1 clock domains"
            ),
            "{output}"
        );
        assert!(output.contains("Prepare model request"), "{output}");
        assert!(output.contains("clock A +100ms"), "{output}");
        assert!(
            output.contains(
                "Provider reported tokens · input 800 · cache read 40 · cache write 10 · output 55"
            ),
            "{output}"
        );
        assert!(output.contains("12 visible tools"), "{output}");
        assert!(
            output.contains("Memory · 90 tokens · 3 sections (runtime estimate)"),
            "{output}"
        );
        assert!(!output.contains("trace"), "{output}");
        assert!(output.contains("2 candidates → 1 selected"), "{output}");
        assert!(
            output.contains("Candidate 1 · selected · model score 90.00%"),
            "{output}"
        );
        assert!(
            output.contains("final prompt injection not measured"),
            "{output}"
        );
    }

    #[test]
    fn marks_missing_terminal_facts_and_unavailable_parallelism_explicitly() {
        let root = fact(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        let output = render(&[root], false, false);
        assert!(output.contains("incomplete"), "{output}");
        assert!(
            output.contains("Observed overlap · unavailable"),
            "{output}"
        );
        assert!(
            output.contains("still running or terminal fact missing"),
            "{output}"
        );
    }

    #[test]
    fn empty_capture_is_reported_as_a_gap_instead_of_a_fake_graph() {
        assert_eq!(
            render(&[], false, false),
            "Explain Analyze · no runtime facts were captured for this turn."
        );
    }

    #[test]
    fn explains_unmeasured_boundaries_without_claiming_total_overlap() {
        let turn_start = fact(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        let mut turn_end = finished(turn_start.clone(), 100);
        turn_end.outcome = Some(ExplainAnalyzeOutcomeV1::Completed);
        turn_end.coverage_gaps = vec![
            ExplainAnalyzeCoverageGapV1::ChildRunIntervals,
            ExplainAnalyzeCoverageGapV1::ToolIoWaitIntervals,
        ];

        let output = render(&[turn_start, turn_end], false, false);
        assert!(
            output.contains("Explain Analyze · partial capture"),
            "{output}"
        );
        assert!(!output.contains("Observed overlap"), "{output}");
        assert!(
            output.contains("Not timed separately · child-run timing · tool I/O wait breakdown"),
            "{output}"
        );
    }

    #[test]
    fn delivery_gap_marks_partial_facts_incomplete() {
        let turn_start = fact(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        let output = render(&[turn_start], false, true);
        assert!(output.contains("Explain Analyze · incomplete"), "{output}");
        assert!(
            output.contains("Observation gap · stream delivery was interrupted"),
            "{output}"
        );
    }
}

/// Same separately attributed auxiliary usage section for text, TUI and HTML.
pub(crate) fn auxiliary_usage_lines(graph: &ExplainAnalyzeGraphV1) -> Vec<String> {
    use std::collections::BTreeMap;
    if graph.auxiliary_capture_conflicted() {
        return vec!["Auxiliary tokens · capture unavailable · conflicting turn/usage facts; no token total inferred; not a truncation claim".into()];
    }
    let conflicts = graph.auxiliary_usage_conflict_count();
    if conflicts > 0 {
        return vec![format!(
            "Auxiliary tokens · capture unavailable · conflicting physical attempt evidence ({conflicts} identities); no token total inferred; not a truncation claim"
        )];
    }
    type GroupKey<'a> = (&'a str, &'a str, &'a str, &'a str, &'a str);
    type Attempts<'a> = Vec<&'a astra_turn_types::ExplainAnalyzeAuxiliaryAttemptV1>;
    let mut groups: BTreeMap<GroupKey<'_>, Attempts<'_>> = BTreeMap::new();
    let truncated = graph.auxiliary_usage_truncated();
    let incomplete_capture = truncated || graph.auxiliary_usage_unavailable();
    for attempt in graph.auxiliary_attempts() {
        groups
            .entry((
                &attempt.provider,
                &attempt.offering_id,
                &attempt.model_name,
                &attempt.purpose,
                &attempt.operation_id,
            ))
            .or_default()
            .push(attempt);
    }
    let mut lines = Vec::new();
    for ((provider, offering, model, purpose, operation), attempts) in groups {
        let provider = if provider == "typesafe" {
            "Jev"
        } else {
            provider
        };
        let purpose = auxiliary_usage_label(operation, purpose);
        let reported = attempts
            .iter()
            .filter_map(|a| a.usage.as_ref())
            .collect::<Vec<_>>();
        let values = if reported.is_empty() {
            "usage unavailable".into()
        } else {
            let lanes = [
                (
                    "in",
                    reported
                        .iter()
                        .map(|u| u.fresh_input_tokens)
                        .collect::<Vec<_>>(),
                ),
                (
                    "cache read",
                    reported.iter().map(|u| u.cache_read_tokens).collect(),
                ),
                (
                    "cache write",
                    reported.iter().map(|u| u.cache_creation_tokens).collect(),
                ),
                ("out", reported.iter().map(|u| u.output_tokens).collect()),
            ];
            lanes
                .into_iter()
                .map(|(name, counts)| {
                    let total = counts.len();
                    let known = counts.into_iter().flatten().collect::<Vec<_>>();
                    if known.is_empty() {
                        format!("{name} unknown")
                    } else {
                        let qualifier = if !incomplete_capture
                            && known.len() == total
                            && reported.len() == attempts.len()
                        {
                            ""
                        } else {
                            "at least "
                        };
                        format!(
                            "{name} {qualifier}{}",
                            known.into_iter().map(u128::from).sum::<u128>()
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let partial = if attempts.iter().any(|a| {
            a.usage_status != astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact
        }) {
            " · partial"
        } else {
            ""
        };
        let scope = if incomplete_capture { " captured" } else { "" };
        lines.push(format!("Auxiliary tokens · {provider} ({model}) · {purpose} · operation {operation} · offering {offering} · {values} · {}/{}{scope} requests reported{partial}",reported.len(),attempts.len()));
    }
    if truncated {
        lines.push("Auxiliary tokens · capture truncated · counts cover captured requests only; all token sums are lower bounds; full usage unknown".into());
    }
    if graph.auxiliary_usage_unavailable() {
        lines.push("Auxiliary tokens · capture unavailable".into());
    }
    lines
}

/// Render the logical auxiliary call intervals and the typed admission result.
/// These facts are intentionally separate from physical token usage: the
/// intervals are measured at the local client boundary, are allowed to
/// overlap, and say nothing about provider-side compute time.
pub(crate) fn auxiliary_details_lines(graph: &ExplainAnalyzeGraphV1) -> Vec<String> {
    let details_by_scope = graph.auxiliary_details();
    if details_by_scope.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for (scope, details) in details_by_scope {
        lines.push(format!("Auxiliary scope · {scope}"));
        let mut calls = details.calls.iter().collect::<Vec<_>>();
        calls.sort_unstable_by(|left, right| {
            left.start_elapsed_ms
                .cmp(&right.start_elapsed_ms)
                .then_with(|| left.call_id.cmp(&right.call_id))
        });
        lines.extend(calls.into_iter().map(|call| {
            format!(
                "Auxiliary timing · {} · call {} · operation {} · stage {} · {} · client outcome {} · starts +{} · logical client interval; overlapping intervals are not added",
                auxiliary_usage_label(&call.operation_id, ""),
                call.call_id,
                call.operation_id,
                call.stage,
                format_ms(call.duration_ms),
                auxiliary_outcome_label(call.outcome),
                format_ms(call.start_elapsed_ms),
            )
        }));
        if details.truncated {
            lines.push(
                "Auxiliary timing · capture truncated · only the bounded set of logical calls is shown"
                    .into(),
            );
        }
        if let Some(admission) = &details.admission {
            let status = match admission.status {
                astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::Accepted => "accepted",
                astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::Rejected => "rejected",
                astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::Unavailable => {
                    "unavailable"
                }
                astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::NotDispatched => {
                    "not dispatched"
                }
            };
            let reason = serde_json::to_string(&admission.reason)
                .unwrap_or_else(|_| r#"{"kind":"unavailable"}"#.to_string());
            let classification = admission
                .classification
                .as_ref()
                .and_then(|result| serde_json::to_string(result).ok())
                .unwrap_or_else(|| "unavailable".to_string());
            let decision = admission
                .decision
                .as_ref()
                .and_then(|result| serde_json::to_string(result).ok())
                .unwrap_or_else(|| "unavailable".to_string());
            lines.push(format!(
                "Admission settlement · status {status} · reason {reason} · classifier result {classification} · reconciled decision {decision}"
            ));
        }
    }
    lines
}

fn auxiliary_outcome_label(outcome: ExplainAnalyzeOutcomeV1) -> &'static str {
    match outcome {
        ExplainAnalyzeOutcomeV1::Completed => "completed",
        ExplainAnalyzeOutcomeV1::Succeeded => "succeeded",
        ExplainAnalyzeOutcomeV1::Failed => "failed",
        ExplainAnalyzeOutcomeV1::Cancelled => "cancelled",
        ExplainAnalyzeOutcomeV1::Interrupted => "interrupted",
        ExplainAnalyzeOutcomeV1::Blocked => "blocked",
        ExplainAnalyzeOutcomeV1::Waiting => "waiting",
        ExplainAnalyzeOutcomeV1::Rejected => "rejected",
        ExplainAnalyzeOutcomeV1::Reused => "reused",
        ExplainAnalyzeOutcomeV1::Suppressed => "suppressed",
        ExplainAnalyzeOutcomeV1::Deferred => "deferred",
        ExplainAnalyzeOutcomeV1::Resolved => "resolved",
        ExplainAnalyzeOutcomeV1::Fallback => "fallback",
        ExplainAnalyzeOutcomeV1::Unavailable => "unavailable",
        ExplainAnalyzeOutcomeV1::Delegated => "delegated",
    }
}

fn auxiliary_usage_label(operation: &str, purpose: &str) -> &'static str {
    match operation {
        "request_judgment" => "Request classification",
        "skill_auto_route" => "Skill selection",
        "work_plan" => "Work planning",
        _ => match purpose {
            "memory_retrieval_rerank" => "Memory judgment",
            "memory_extraction" => "Memory extraction",
            "introspection" => "Request analysis",
            "verification_judge" => "Verification",
            "reflection" => "Reflection",
            "required_compaction" => "Context summary",
            _ => "Auxiliary inference",
        },
    }
}

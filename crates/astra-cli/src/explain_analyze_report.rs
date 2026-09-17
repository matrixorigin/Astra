//! Plain-text Explain Analyze rendering over canonical execution facts.
//!
//! This renderer consumes only typed lifecycle facts. It never reconstructs
//! work from trace prose, CLI wall-clock timers, or user/provider payloads.

use std::collections::{BTreeSet, HashMap, HashSet};

use astra_turn_types::{
    ExplainAnalyzeEventV1, ExplainAnalyzeGraphV1, ExplainAnalyzeNodeKindV1,
    ExplainAnalyzeOutcomeV1, ExplainAnalyzeProjectionDiagnosticCodeV1, ExplainAnalyzeUsageBasisV1,
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

fn alpha_label(mut ordinal: usize) -> String {
    let mut label = String::new();
    loop {
        label.insert(
            0,
            char::from(b'A' + u8::try_from(ordinal % 26).unwrap_or(0)),
        );
        ordinal /= 26;
        if ordinal == 0 {
            return label;
        }
        ordinal -= 1;
    }
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

fn diagnostic_label(code: ExplainAnalyzeProjectionDiagnosticCodeV1) -> &'static str {
    use ExplainAnalyzeProjectionDiagnosticCodeV1::*;
    match code {
        ConflictingFact => "conflicting runtime facts",
        DependencyCycle => "cyclic dependency",
        InvalidEvent => "invalid runtime fact",
        MissingDependency => "dependency was not observed",
        MissingParent => "parent stage was not observed",
        ParentCycle => "cyclic stage hierarchy",
        UnresolvedTerminalNode => "stage did not reach a recorded end",
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

fn format_ms(ms: u64) -> String {
    if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else if tokens >= 1_000 {
        format!("{:.2}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
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
            assembly: Some(ExplainAnalyzeContextAssemblyV1 {
                basis: ExplainAnalyzeContextAssemblyBasisV1::RuntimeTextEstimate,
                sources: vec![ExplainAnalyzeContextSourceV1 {
                    kind: ExplainAnalyzeContextSourceKindV1::Memory,
                    section_count: 3,
                    estimated_tokens: 90,
                }],
            }),
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

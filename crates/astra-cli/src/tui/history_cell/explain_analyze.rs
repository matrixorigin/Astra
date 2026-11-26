//! Compact TUI projection of canonical Explain Analyze facts.
use std::{
    any::Any,
    collections::{BTreeSet, HashMap, HashSet},
};

use crate::explain_analyze_format::{
    compact_usage, decision_detail_line, format_ms, format_tokens_u128, provider_usage_summary,
};
use astra_turn_types::ExplainAnalyzeGraphV1;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{HistoryCell, truncate_by_width as truncate};

#[derive(Debug, Clone)]
pub(crate) struct ExplainAnalyzeCell {
    graph: ExplainAnalyzeGraphV1,
    delivery_degraded: bool,
    verbose: bool,
}

impl ExplainAnalyzeCell {
    pub(crate) fn new(
        graph: ExplainAnalyzeGraphV1,
        delivery_degraded: bool,
        verbose: bool,
    ) -> Self {
        Self {
            graph,
            delivery_degraded,
            verbose,
        }
    }
}

impl HistoryCell for ExplainAnalyzeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        render_graph(&self.graph, width, self.delivery_degraded, self.verbose)
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn render_graph(
    graph: &ExplainAnalyzeGraphV1,
    width: u16,
    delivery_degraded: bool,
    verbose: bool,
) -> Vec<Line<'static>> {
    use astra_turn_types::{ExplainAnalyzeGraphIntegrityV1, ExplainAnalyzeNodeKindV1};

    let theme = crate::tui::theme::current();
    let width = usize::from(width.max(1));
    let coverage_gaps = graph.coverage_gaps();
    let clock_labels = clock_labels(graph);
    let concurrency = graph.max_concurrency();
    let integrity = if delivery_degraded {
        "incomplete · stream gap"
    } else if graph.integrity() != ExplainAnalyzeGraphIntegrityV1::Consistent
        || graph.nodes().iter().any(|node| !node.terminal_observed)
    {
        "incomplete"
    } else if !coverage_gaps.is_empty() {
        "partial capture"
    } else {
        "recorded"
    };

    let mut header = String::from("Explain Analyze");
    let mut turns = graph
        .nodes()
        .iter()
        .filter(|node| node.kind == ExplainAnalyzeNodeKindV1::Turn);
    if let Some(turn) = turns.next()
        && turns.next().is_none()
        && turn.terminal_observed
        && !turn.conflicted
    {
        if let Some(outcome) = turn.outcome {
            header.push_str(&format!(" · {}", outcome_label(outcome)));
        }
        if let Some(wall) = turn.duration_ms {
            header.push_str(&format!(" · {} wall", format_ms(wall)));
        }
    }
    header.push_str(&format!(" · {integrity} · {} stages", graph.nodes().len()));
    // Keep coverage and elapsed time readable even when the heading wraps.
    let header_style = Style::default().fg(theme.accent).bold();
    let mut lines = wrap_words(&header, width)
        .into_iter()
        .map(|part| Line::styled(truncate(&part, width), header_style))
        .collect::<Vec<_>>();
    if verbose || concurrency.is_some_and(|count| count > 1) || clock_labels.len() > 1 {
        let overlap = match concurrency {
            Some(count) if integrity != "recorded" => {
                format!("Observed overlap ≥{count}")
            }
            Some(count) if clock_labels.len() > 1 => format!("Peak parallel per clock {count}"),
            Some(count) => format!("Peak parallel {count}"),
            None => "Parallel overlap unavailable".to_string(),
        };
        let clocks = if clock_labels.len() == 1 {
            "clock"
        } else {
            "clocks"
        };
        push_wrapped_detail(
            &mut lines,
            &format!("{overlap} · {} {clocks}", clock_labels.len()),
            width,
            Style::default().fg(theme.dim),
        );
    }
    if let Some(summary) = provider_usage_summary(graph) {
        push_wrapped_detail(&mut lines, &summary, width, Style::default().fg(theme.dim));
    }
    if let Some((_, step)) = graph.longest_measured_step() {
        push_wrapped_detail(
            &mut lines,
            &format!(
                "Longest measured step · {} · {}",
                step.label,
                step.duration_ms.map(format_ms).unwrap_or_default()
            ),
            width,
            Style::default().fg(theme.dim),
        );
    }
    if width >= 60 {
        let columns = wide_metrics(
            "Start",
            "Duration",
            (width >= 96).then_some("Provider tokens"),
        );
        let stage = width.saturating_sub(columns.width() + 19);
        lines.push(Line::styled(
            format!("{:<stage$}{columns}  Status", "Stage"),
            Style::default().fg(theme.dim),
        ));
    }

    let mut seen = HashSet::new();
    let mut roots = graph.roots().collect::<Vec<_>>();
    roots.sort_unstable();
    let mut roots = roots.into_iter().peekable();
    let mut orphans = 0..graph.nodes().len();
    let mut orphan_heading = false;
    let unresolved_nodes = graph
        .diagnostics()
        .iter()
        .filter(|diagnostic| {
            diagnostic.code
                == astra_turn_types::ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode
        })
        .filter_map(|diagnostic| diagnostic.node_id.as_deref())
        .collect::<HashSet<_>>();
    let mut stack = Vec::new();

    loop {
        if stack.is_empty() {
            if let Some(root) = roots.find(|index| !seen.contains(index)) {
                stack.push((root, Vec::new(), roots.peek().is_none()));
            } else if let Some(orphan) = orphans.find(|index| !seen.contains(index)) {
                if !orphan_heading {
                    lines.push(Line::from(Span::styled(
                        truncate("Unlinked stages", width),
                        Style::default().fg(theme.warn).bold(),
                    )));
                    orphan_heading = true;
                }
                stack.push((orphan, Vec::new(), true));
            } else {
                break;
            }
        }

        while let Some((index, mut ancestors, last)) = stack.pop() {
            if !seen.insert(index) {
                continue;
            }
            let node = &graph.nodes()[index];
            let prefix = tree_prefix(&ancestors, last);
            let duration = node
                .duration_ms
                .map(format_ms)
                .unwrap_or_else(|| "—".into());
            let state = node_state(
                node,
                unresolved_nodes.contains(node.node_id.as_str()),
                theme,
            );
            let offset = if clock_labels.len() > 1 {
                format!(
                    "{}+{}",
                    clock_labels[&node.clock_domain_id],
                    format_ms(node.start_elapsed_ms)
                )
            } else {
                format!("+{}", format_ms(node.start_elapsed_ms))
            };
            let (row, compact) = node_row(node, &prefix, &offset, &duration, state, width, theme);
            lines.push(row);

            ancestors.push(!last);
            let mut detail = |text: &str, style| {
                push_wrapped_node_detail(&mut lines, text, width, style, &ancestors);
            };
            if compact {
                detail(
                    &format!("State · {}", state.0),
                    Style::default().fg(state.1),
                );
            }
            if let Some(decision) = &node.decision_detail {
                detail(
                    &decision_detail_line(decision),
                    Style::default().fg(theme.warn),
                );
            }
            if let Some(usage) = &node.usage
                && (verbose
                    || width < 96
                    || usage.cache_read_tokens.is_some()
                    || usage.cache_creation_tokens.is_some()
                    || compact_usage(usage).width() > 23)
            {
                let lanes = [
                    ("in", usage.fresh_input_tokens),
                    ("cache read", usage.cache_read_tokens),
                    ("cache write", usage.cache_creation_tokens),
                    ("out", usage.output_tokens),
                ]
                .into_iter()
                .filter_map(|(label, value)| Some(format!("{label} {}", format_tokens(value?))))
                .collect::<Vec<_>>()
                .join(" · ");
                if !lanes.is_empty() {
                    use astra_turn_types::ExplainAnalyzeUsageBasisV1::*;
                    let basis = match usage.basis {
                        ProviderExact => "provider reported",
                        ProviderPartial => "partial provider report",
                        RuntimeEstimated => "runtime estimate",
                    };
                    detail(
                        &format!("{basis} · {lanes} tokens"),
                        Style::default().fg(theme.dim),
                    );
                }
            }
            if let Some(context) = &node.context {
                let style = Style::default().fg(theme.dim);
                if let Some(budget) = &context.budget {
                    if verbose {
                        detail("Request budget · pre-provider estimate", style.bold());
                        detail(
                            &format!(
                                "input {} / limit {} · system {} · tool schemas {} · requested output {}",
                                format_tokens(budget.estimated_input_tokens),
                                format_tokens(budget.effective_input_limit_tokens),
                                format_tokens(budget.estimated_system_tokens),
                                format_tokens(budget.tool_schema_tokens),
                                format_tokens(budget.requested_output_tokens),
                            ),
                            style,
                        );
                        detail(
                            &format!(
                                "protocol reserve {} · model context {} · visible tools {}",
                                format_tokens(budget.reserved_protocol_tokens),
                                format_tokens(budget.model_context_limit_tokens),
                                budget.visible_tool_count,
                            ),
                            style,
                        );
                    } else {
                        detail(
                            &format!(
                                "Request budget · pre-provider estimate · input {} / limit {} · output {} · {} tools",
                                format_tokens(budget.estimated_input_tokens),
                                format_tokens(budget.effective_input_limit_tokens),
                                format_tokens(budget.requested_output_tokens),
                                budget.visible_tool_count,
                            ),
                            style,
                        );
                    }
                }
                if let Some(assembly) = &context.assembly {
                    if verbose {
                        detail("Context sources · runtime text estimate", style.bold());
                        for source in &assembly.sources {
                            detail(
                                &format!(
                                    "{} · {} tokens · {} sections",
                                    source_label(source.kind),
                                    format_tokens(source.estimated_tokens),
                                    source.section_count,
                                ),
                                style,
                            );
                        }
                    } else if !assembly.sources.is_empty() {
                        let sources = assembly
                            .sources
                            .iter()
                            .map(|source| {
                                format!(
                                    "{} {} tokens",
                                    source_label(source.kind),
                                    format_tokens(source.estimated_tokens)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(" · ");
                        detail(
                            &format!("Context sources · runtime text estimate · {sources}"),
                            style,
                        );
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
                            .map(|dependency| dependency.label.as_str())
                            .unwrap_or("unrecorded stage")
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                detail(
                    &format!("Dependencies · {dependencies}"),
                    Style::default().fg(theme.dim),
                );
            }
            if verbose && !node.coverage_gaps.is_empty() {
                let gaps = node
                    .coverage_gaps
                    .iter()
                    .map(|gap| gap.label())
                    .collect::<Vec<_>>()
                    .join(" · ");
                detail(
                    &format!("Coverage · not measured separately: {gaps}"),
                    Style::default().fg(theme.warn),
                );
            }

            let mut children = graph.children(index).to_vec();
            if children
                .iter()
                .all(|child| graph.nodes()[*child].clock_domain_id == node.clock_domain_id)
            {
                children.sort_by_key(|child| graph.nodes()[*child].start_elapsed_ms);
            }
            for (position, child) in children.iter().enumerate().rev() {
                stack.push((*child, ancestors.clone(), position + 1 == children.len()));
            }
        }
    }

    if !coverage_gaps.is_empty() {
        let visible = if verbose { coverage_gaps.len() } else { 2 };
        let labels = coverage_gaps
            .iter()
            .take(visible)
            .map(|gap| gap.label())
            .collect::<Vec<_>>()
            .join(" · ");
        let more = coverage_gaps.len().saturating_sub(visible);
        let suffix = if more > 0 {
            format!(" · {more} more in report")
        } else {
            String::new()
        };
        push_wrapped_detail(
            &mut lines,
            &format!("Not timed separately · {labels}{suffix}"),
            width,
            Style::default().fg(theme.warn),
        );
    }
    for summary in crate::explain_analyze_report::auxiliary_usage_lines_with_detail(graph, verbose)
        .into_iter()
        .chain(crate::explain_analyze_report::auxiliary_details_lines(
            graph,
        ))
    {
        push_wrapped_detail(&mut lines, &summary, width, Style::default().fg(theme.dim));
    }
    if !graph.diagnostics().is_empty() {
        let summary = graph
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic_label(diagnostic.code))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(3)
            .collect::<Vec<_>>()
            .join(" · ");
        push_wrapped_detail(
            &mut lines,
            &format!(
                "Review measurements · {summary}{}",
                if graph.diagnostics().len() > 3 {
                    " · more"
                } else {
                    ""
                },
            ),
            width,
            Style::default().fg(theme.warn),
        );
    }
    lines
}

fn outcome_label(outcome: astra_turn_types::ExplainAnalyzeOutcomeV1) -> &'static str {
    use astra_turn_types::ExplainAnalyzeOutcomeV1::*;
    match outcome {
        Completed | Succeeded | Resolved => "Completed",
        Failed | Rejected => "Failed",
        Cancelled => "Cancelled",
        Interrupted => "Interrupted",
        Blocked | Waiting | Deferred => "Waiting",
        Reused => "Reused",
        Suppressed => "Suppressed",
        Fallback => "Fallback",
        Unavailable => "Unavailable",
        Delegated => "Delegated",
    }
}

fn source_label(kind: astra_turn_types::ExplainAnalyzeContextSourceKindV1) -> &'static str {
    use astra_turn_types::ExplainAnalyzeContextSourceKindV1::*;
    match kind {
        Identity => "Identity",
        SelfModel => "Self model",
        ProjectContext => "Project",
        DeferredTools => "Deferred tools",
        AvailableSkills => "Available skills",
        Memory => "Memory",
        WorkingMemory => "Working memory",
        History => "History",
        Constraints => "Constraints",
        Skills => "Skills",
        RuntimeIdentity => "Runtime identity",
        RuntimeVolatile => "Runtime state",
    }
}

/// Append a complete factual detail row, wrapping at word boundaries instead
/// of silently dropping the tail of context and token measurements.
fn push_wrapped_detail(lines: &mut Vec<Line<'static>>, detail: &str, width: usize, style: Style) {
    let indent = if width >= 8 {
        2
    } else if width >= 4 {
        1
    } else {
        0
    };
    let first_prefix = " ".repeat(indent);
    let continuation_prefix = " ".repeat((indent * 2).min(width.saturating_sub(1)));
    let content_width = width.saturating_sub(continuation_prefix.width()).max(1);
    let wrapped = wrap_words(detail, content_width);
    for (index, part) in wrapped.into_iter().enumerate() {
        let prefix = if index == 0 {
            first_prefix.as_str()
        } else {
            continuation_prefix.as_str()
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{}", truncate(&part, content_width)),
            style,
        )));
    }
}

/// Render a node-owned detail row as a real child of that node. Details used
/// to start with two spaces regardless of the node depth, which made frozen
/// reports look like a second top-level report rather than one tree. The
/// prefix is shared by every wrapped line so the relationship stays visible
/// at narrow terminal widths too.
fn push_wrapped_node_detail(
    lines: &mut Vec<Line<'static>>,
    detail: &str,
    width: usize,
    style: Style,
    ancestors_with_following_sibling: &[bool],
) {
    let prefix = fit_tree_prefix(&detail_prefix(ancestors_with_following_sibling), width / 3);
    let prefix_width = prefix.width();
    let available = width.saturating_sub(prefix_width).max(1);
    let wrapped = wrap_words(detail, available);
    let continuation = " ".repeat(prefix_width);
    for (index, part) in wrapped.into_iter().enumerate() {
        let row_prefix = if index == 0 { &prefix } else { &continuation };
        lines.push(Line::from(Span::styled(
            format!("{row_prefix}{}", truncate(&part, available)),
            style,
        )));
    }
}

fn wrap_words(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in value.split_whitespace() {
        let word_width = word.width();
        if word_width > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            for ch in word.chars() {
                let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if !current.is_empty() && current_width + char_width > width {
                    lines.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                current.push(ch);
                current_width += char_width;
            }
            continue;
        }

        let separator_width = usize::from(!current.is_empty());
        if !current.is_empty() && current_width + separator_width + word_width > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn clock_labels(graph: &ExplainAnalyzeGraphV1) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    for node in graph.nodes() {
        if !labels.contains_key(&node.clock_domain_id) {
            labels.insert(
                node.clock_domain_id.clone(),
                format!("C{}", labels.len() + 1),
            );
        }
    }
    labels
}

fn tree_prefix(ancestors_with_following_sibling: &[bool], last: bool) -> String {
    let mut prefix = String::new();
    for has_following_sibling in ancestors_with_following_sibling.iter().take(6) {
        prefix.push_str(if *has_following_sibling { "│ " } else { "  " });
    }
    if !ancestors_with_following_sibling.is_empty() {
        prefix.push_str(if last { "└─ " } else { "├─ " });
    }
    prefix
}

fn detail_prefix(ancestors_with_following_sibling: &[bool]) -> String {
    let mut prefix = tree_prefix(ancestors_with_following_sibling, true);
    if let Some(index) = prefix.rfind("└─ ") {
        prefix.replace_range(index.., "·  ");
    }
    prefix
}

fn node_row(
    node: &astra_turn_types::ExplainAnalyzeProjectedNodeV1,
    prefix: &str,
    offset: &str,
    duration: &str,
    (state, state_color, state_short): (&str, Color, &str),
    width: usize,
    theme: &crate::tui::theme::Theme,
) -> (Line<'static>, bool) {
    let muted = Style::default().fg(theme.dim);
    let label_style = if matches!(
        node.kind,
        astra_turn_types::ExplainAnalyzeNodeKindV1::Run
            | astra_turn_types::ExplainAnalyzeNodeKindV1::Turn
    ) {
        Style::default().fg(theme.fg).bold()
    } else {
        Style::default().fg(theme.fg)
    };
    let state_style = Style::default().fg(state_color);
    if width >= 60 {
        let offset = truncate(offset, 13);
        let duration = truncate(duration, 9);
        let tokens = (width >= 96).then(|| {
            let tokens = node
                .usage
                .as_ref()
                .map(compact_usage)
                .unwrap_or_else(|| "—".into());
            truncate(&tokens, 23)
        });
        let metrics = wide_metrics(&offset, &duration, tokens.as_deref());
        let state_cell = format!("  {state:<17}");
        let label_room = width.saturating_sub(metrics.width() + state_cell.width());
        let prefix = fit_tree_prefix(prefix, label_room / 2);
        let label = truncate(&node.label, label_room.saturating_sub(prefix.width()));
        let padding = " ".repeat(label_room.saturating_sub(prefix.width() + label.width()));
        return (
            Line::from(vec![
                Span::styled(prefix, muted),
                Span::styled(label, label_style),
                Span::styled(format!("{padding}{metrics}"), muted),
                Span::styled(state_cell, state_style),
            ]),
            false,
        );
    }

    let compact_prefix = prefix
        .replace("│ ", "│")
        .replace("  ", " ")
        .replace("├─ ", "├")
        .replace("└─ ", "└");
    let time = truncate(duration, 9);
    let suffix = format!(" {time} {state_short}");
    if width <= suffix.width() + 2 {
        return (
            Line::styled(truncate(&node.label, width), label_style),
            true,
        );
    }
    let label_room = width - suffix.width();
    let prefix = fit_tree_prefix(&compact_prefix, label_room / 2);
    (
        Line::from(vec![
            Span::styled(
                truncate(&format!("{prefix}{}", node.label), label_room),
                label_style,
            ),
            Span::styled(format!(" {time} "), muted),
            Span::styled(state_short.to_owned(), state_style),
        ]),
        true,
    )
}

fn wide_metrics(offset: &str, duration: &str, tokens: Option<&str>) -> String {
    let mut cells = format!("  {offset:>13}  {duration:>9}");
    if let Some(tokens) = tokens {
        cells.push_str(&format!("  {tokens:>23}"));
    }
    cells
}

fn fit_tree_prefix(prefix: &str, max: usize) -> String {
    if prefix.width() <= max {
        return prefix.to_owned();
    }
    if max < 2 {
        return String::new();
    }
    let start = prefix
        .char_indices()
        .rev()
        .nth(max - 2)
        .map_or(0, |(i, _)| i);
    format!("…{}", &prefix[start..])
}

fn node_state(
    node: &astra_turn_types::ExplainAnalyzeProjectedNodeV1,
    unresolved_terminal: bool,
    theme: &crate::tui::theme::Theme,
) -> (&'static str, Color, &'static str) {
    if node.conflicted {
        return ("Conflicting facts", theme.warn, "conflict");
    }
    if unresolved_terminal {
        return ("End not recorded", theme.warn, "open");
    }
    if !node.terminal_observed {
        return ("End not recorded", theme.dim, "open");
    }
    let Some(outcome) = node.outcome else {
        return ("Finished", theme.dim, "done");
    };
    let color = match outcome {
        astra_turn_types::ExplainAnalyzeOutcomeV1::Failed
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Rejected
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Interrupted => theme.error,
        astra_turn_types::ExplainAnalyzeOutcomeV1::Cancelled
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Blocked
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Waiting
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Deferred => theme.warn,
        astra_turn_types::ExplainAnalyzeOutcomeV1::Completed
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Succeeded
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Resolved => theme.success,
        _ => theme.dim,
    };
    let short = match outcome {
        astra_turn_types::ExplainAnalyzeOutcomeV1::Completed
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Succeeded
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Resolved => "done",
        astra_turn_types::ExplainAnalyzeOutcomeV1::Failed
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Rejected
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Interrupted => "fail",
        astra_turn_types::ExplainAnalyzeOutcomeV1::Cancelled => "cancel",
        astra_turn_types::ExplainAnalyzeOutcomeV1::Blocked
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Waiting
        | astra_turn_types::ExplainAnalyzeOutcomeV1::Deferred => "wait",
        astra_turn_types::ExplainAnalyzeOutcomeV1::Reused => "reuse",
        astra_turn_types::ExplainAnalyzeOutcomeV1::Suppressed => "skip",
        astra_turn_types::ExplainAnalyzeOutcomeV1::Fallback => "fallback",
        astra_turn_types::ExplainAnalyzeOutcomeV1::Unavailable => "n/a",
        astra_turn_types::ExplainAnalyzeOutcomeV1::Delegated => "delegate",
    };
    (outcome_label(outcome), color, short)
}

fn diagnostic_label(
    code: astra_turn_types::ExplainAnalyzeProjectionDiagnosticCodeV1,
) -> &'static str {
    use astra_turn_types::ExplainAnalyzeProjectionDiagnosticCodeV1::*;
    match code {
        ConflictingFact => "conflicting events",
        DependencyCycle => "dependency loop",
        InvalidEvent => "invalid event",
        MissingDependency => "dependency not recorded",
        MissingParent => "parent not recorded",
        ParentCycle => "parent loop",
        UnresolvedTerminalNode => "unfinished stage",
    }
}

fn format_tokens(value: u64) -> String {
    format_tokens_u128(u128::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        EXPLAIN_ANALYZE_SCHEMA_VERSION, ExplainAnalyzeContextAssemblyBasisV1,
        ExplainAnalyzeContextAssemblyV1, ExplainAnalyzeContextBudgetBasisV1,
        ExplainAnalyzeContextBudgetV1, ExplainAnalyzeContextMetricsV1,
        ExplainAnalyzeContextSourceKindV1, ExplainAnalyzeContextSourceV1, ExplainAnalyzeEventV1,
        ExplainAnalyzeNodeKindV1, ExplainAnalyzeOutcomeV1, ExplainAnalyzeTokenUsageV1,
        ExplainAnalyzeTransitionV1, ExplainAnalyzeUsageBasisV1,
    };

    #[test]
    fn conflicting_auxiliary_usage_is_visible_in_settled_cell() {
        use astra_turn_types::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1,
        };
        let mut graph = ExplainAnalyzeGraphV1::default();
        for (index, count) in [731, 947].into_iter().enumerate() {
            let mut event = finished(
                &format!("finish-{index}"),
                &format!("turn-{index}"),
                None,
                ExplainAnalyzeNodeKindV1::Turn,
                "clock",
                0,
                10,
                ExplainAnalyzeOutcomeV1::Succeeded,
                None,
                None,
            );
            event.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: vec![ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: "same-attempt".into(),
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
            graph.apply(event);
        }
        graph.finish_ingest();
        let output = text(&ExplainAnalyzeCell::new(graph, false, false).display_lines(400));
        assert!(
            output.contains(
                "Judgment usage · unavailable · 1 conflicting physical measurement(s); token total unknown"
            ),
            "{output}"
        );
        assert!(!output.contains("731") && !output.contains("947"));
    }

    fn started(
        event_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        kind: ExplainAnalyzeNodeKindV1,
        clock_domain_id: &str,
        elapsed_ms: u64,
    ) -> ExplainAnalyzeEventV1 {
        event(
            event_id,
            node_id,
            parent_node_id,
            kind,
            clock_domain_id,
            ExplainAnalyzeTransitionV1::Started,
            elapsed_ms,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finished(
        event_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        kind: ExplainAnalyzeNodeKindV1,
        clock_domain_id: &str,
        start_ms: u64,
        duration_ms: u64,
        outcome: ExplainAnalyzeOutcomeV1,
        usage: Option<ExplainAnalyzeTokenUsageV1>,
        context: Option<ExplainAnalyzeContextMetricsV1>,
    ) -> ExplainAnalyzeEventV1 {
        event(
            event_id,
            node_id,
            parent_node_id,
            kind,
            clock_domain_id,
            ExplainAnalyzeTransitionV1::Finished,
            start_ms + duration_ms,
            Some(start_ms),
            Some(duration_ms),
            Some(outcome),
            usage,
            context,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        event_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        kind: ExplainAnalyzeNodeKindV1,
        clock_domain_id: &str,
        transition: ExplainAnalyzeTransitionV1,
        elapsed_ms: u64,
        start_elapsed_ms: Option<u64>,
        duration_ms: Option<u64>,
        outcome: Option<ExplainAnalyzeOutcomeV1>,
        usage: Option<ExplainAnalyzeTokenUsageV1>,
        context: Option<ExplainAnalyzeContextMetricsV1>,
    ) -> ExplainAnalyzeEventV1 {
        let is_provider = kind == ExplainAnalyzeNodeKindV1::ProviderAttempt;
        ExplainAnalyzeEventV1 {
            auxiliary_usage: None,
            auxiliary_details: None,
            schema_version: EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: event_id.into(),
            run_id: "run-1".into(),
            turn_id: "turn-1".into(),
            node_id: node_id.into(),
            parent_node_id: parent_node_id.map(str::to_owned),
            dependency_node_ids: Vec::new(),
            producer_id: "server-loop".into(),
            clock_domain_id: clock_domain_id.into(),
            kind,
            round_index: is_provider.then_some(0),
            attempt_index: is_provider.then_some(0),
            label: node_id.replace('-', " "),
            transition,
            elapsed_ms,
            start_elapsed_ms,
            duration_ms,
            outcome,
            decision_detail: None,
            usage,
            context,
            coverage_gaps: Vec::new(),
        }
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn example_graph() -> ExplainAnalyzeGraphV1 {
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(started(
            "turn-start",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            "clock-1",
            0,
        ));
        graph.apply(finished(
            "prep-finish",
            "preparation",
            Some("turn"),
            ExplainAnalyzeNodeKindV1::Preparation,
            "clock-1",
            10,
            5,
            ExplainAnalyzeOutcomeV1::Completed,
            None,
            Some(ExplainAnalyzeContextMetricsV1 {
                budget: Some(ExplainAnalyzeContextBudgetV1 {
                    basis: ExplainAnalyzeContextBudgetBasisV1::PreProviderEstimate,
                    estimated_input_tokens: 123,
                    estimated_system_tokens: 20,
                    tool_schema_tokens: 3,
                    requested_output_tokens: 400,
                    reserved_protocol_tokens: 8,
                    effective_input_limit_tokens: 1_000,
                    model_context_limit_tokens: 2_000,
                    visible_tool_count: 2,
                }),
                assembly: None,
            }),
        ));
        graph.apply(finished(
            "assembly-finish",
            "assembly",
            Some("preparation"),
            ExplainAnalyzeNodeKindV1::ContextAssembly,
            "clock-1",
            16,
            2,
            ExplainAnalyzeOutcomeV1::Completed,
            None,
            Some(ExplainAnalyzeContextMetricsV1 {
                budget: None,
                assembly: Some(Box::new(ExplainAnalyzeContextAssemblyV1 {
                    basis: ExplainAnalyzeContextAssemblyBasisV1::RuntimeTextEstimate,
                    sources: vec![ExplainAnalyzeContextSourceV1 {
                        kind: ExplainAnalyzeContextSourceKindV1::Memory,
                        section_count: 4,
                        estimated_tokens: 90,
                    }],
                })),
            }),
        ));
        let mut provider = finished(
            "provider-finish",
            "provider",
            Some("turn"),
            ExplainAnalyzeNodeKindV1::ProviderAttempt,
            "clock-1",
            1_200,
            420,
            ExplainAnalyzeOutcomeV1::Failed,
            Some(ExplainAnalyzeTokenUsageV1 {
                basis: ExplainAnalyzeUsageBasisV1::ProviderPartial,
                fresh_input_tokens: Some(1_200),
                cache_read_tokens: Some(240),
                cache_creation_tokens: Some(30),
                output_tokens: Some(40),
            }),
            None,
        );
        provider.decision_detail = astra_turn_types::ExplainAnalyzeDecisionDetailV1::failure(
            astra_core::ErrorKind::RateLimit,
            ExplainAnalyzeOutcomeV1::Failed,
        );
        graph.apply(provider);
        graph
    }

    #[test]
    fn settled_tree_explains_time_usage_and_context_without_mixing_estimates() {
        let mut graph = example_graph();
        graph.finish_ingest();
        let rendered = text(&ExplainAnalyzeCell::new(graph, false, true).display_lines(120));

        assert!(rendered.contains("+1.2s"), "{rendered}");
        assert!(rendered.contains("420ms"), "{rendered}");
        assert!(rendered.contains("Failed"), "{rendered}");
        assert!(rendered.contains("Error · rate limit"), "{rendered}");
        assert!(
            rendered.contains("Provider tokens · in 1,200"),
            "{rendered}"
        );
        assert!(rendered.contains("cache read 240"), "{rendered}");
        assert!(rendered.contains("cache write 30"), "{rendered}");
        assert!(rendered.contains("partial provider report"), "{rendered}");
        assert!(
            rendered.contains("Request budget · pre-provider estimate"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Context sources · runtime text estimate")
                && rendered.contains("Memory · 90 tokens · 4 sections"),
            "{rendered}"
        );
    }

    #[test]
    fn wrapped_context_details_keep_every_measurement_readable_at_narrow_widths() {
        let graph = example_graph();
        let lines = ExplainAnalyzeCell::new(graph, false, true).display_lines(48);
        let rendered = text(&lines);

        for expected in [
            "input 123 / limit 1,000",
            "system 20",
            "tool schemas 3",
            "requested output 400",
            "protocol reserve 8",
            "model context 2,000",
            "visible tools 2",
            "Memory · 90 tokens · 4 sections",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
                <= 48
        }));
    }

    #[test]
    fn deep_tree_keeps_longest_step_duration_and_status_inside_narrow_view() {
        let mut graph = ExplainAnalyzeGraphV1::default();
        for i in 0..16 {
            let id = format!("stage-{i}");
            let parent = (i > 0).then(|| format!("stage-{}", i - 1));
            graph.apply(finished(
                &format!("{id}-finish"),
                &id,
                parent.as_deref(),
                ExplainAnalyzeNodeKindV1::ToolCall,
                "clock",
                0,
                if i == 15 { 420 } else { 12 },
                if i == 15 {
                    ExplainAnalyzeOutcomeV1::Failed
                } else {
                    ExplainAnalyzeOutcomeV1::Succeeded
                },
                None,
                None,
            ));
        }
        graph.finish_ingest();
        let cell = ExplainAnalyzeCell::new(graph, false, false);
        let narrow = cell.display_lines(22);
        assert!(narrow.iter().all(|line| line.width() <= 22));
        assert!(text(&narrow).contains("420ms fail"));
        assert!(
            text(&cell.display_lines(120)).contains("Longest measured step · stage 15 · 420ms")
        );
    }

    #[test]
    fn frozen_snapshot_does_not_invent_an_end_for_an_open_turn() {
        let mut graph = example_graph();
        graph.finish_ingest();
        let cell = ExplainAnalyzeCell::new(graph, false, false);
        let rendered = text(&cell.display_lines(120));
        let turn_line = rendered
            .lines()
            .find(|line| line.contains("turn  "))
            .expect("turn row remains in the tree");

        assert!(rendered.contains("incomplete"), "{rendered}");
        assert!(turn_line.contains("End not recorded"), "{turn_line}");
        assert!(!turn_line.contains("Completed"), "{turn_line}");
    }

    #[test]
    fn terminal_coverage_gaps_are_visible_in_the_tree() {
        let mut graph = example_graph();
        let mut turn_terminal = finished(
            "turn-finished",
            "turn",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            "clock-1",
            0,
            1_600,
            ExplainAnalyzeOutcomeV1::Completed,
            None,
            None,
        );
        turn_terminal.coverage_gaps = vec![
            astra_turn_types::ExplainAnalyzeCoverageGapV1::ChildRunIntervals,
            astra_turn_types::ExplainAnalyzeCoverageGapV1::ToolIoWaitIntervals,
        ];
        graph.apply(turn_terminal);
        graph.finish_ingest();

        let rendered = text(&ExplainAnalyzeCell::new(graph, false, false).display_lines(120));
        assert!(
            rendered.contains("Explain Analyze · Completed · 1.6s wall"),
            "{rendered}"
        );
        assert!(rendered.contains("Failed"), "{rendered}");
        assert!(rendered.contains("partial capture"), "{rendered}");
        assert!(
            rendered.contains("Not timed separately · child-run timing · tool I/O wait breakdown",),
            "{rendered}"
        );
    }

    #[test]
    fn independent_clock_offsets_are_labeled_in_the_settled_tree() {
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(started(
            "a-start",
            "turn-a",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            "clock-a",
            0,
        ));
        graph.apply(started(
            "b-start",
            "turn-b",
            None,
            ExplainAnalyzeNodeKindV1::Turn,
            "clock-b",
            500,
        ));
        graph.finish_ingest();
        let rendered = ExplainAnalyzeCell::new(graph, false, false).display_lines(80);
        let output = text(&rendered);

        assert!(output.contains("2 clocks"), "{output}");
        assert!(output.contains("C1+0ms"), "{output}");
        assert!(output.contains("C2+500ms"), "{output}");
        assert!(rendered.iter().all(|line| {
            line.spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
                <= 80
        }));
    }
}

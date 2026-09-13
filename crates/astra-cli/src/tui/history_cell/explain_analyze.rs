//! Compact TUI projection of canonical Explain Analyze facts.
use std::{
    any::Any,
    collections::{BTreeSet, HashMap, HashSet},
};

use astra_turn_types::ExplainAnalyzeGraphV1;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::HistoryCell;

#[derive(Debug, Clone)]
pub(crate) struct ExplainAnalyzeCell {
    graph: ExplainAnalyzeGraphV1,
    delivery_degraded: bool,
}

impl ExplainAnalyzeCell {
    pub(crate) fn new(graph: ExplainAnalyzeGraphV1, delivery_degraded: bool) -> Self {
        Self {
            graph,
            delivery_degraded,
        }
    }

    pub(crate) fn live_lines(
        graph: &ExplainAnalyzeGraphV1,
        width: u16,
        max_rows: u16,
        delivery_degraded: bool,
    ) -> Vec<Line<'static>> {
        let limit = usize::from(max_rows.max(2));
        render_graph(graph, width, true, Some(limit), delivery_degraded)
    }
}

impl HistoryCell for ExplainAnalyzeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        render_graph(&self.graph, width, false, None, self.delivery_degraded)
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
    live: bool,
    node_limit: Option<usize>,
    delivery_degraded: bool,
) -> Vec<Line<'static>> {
    let theme = crate::tui::theme::current();
    let integrity = if delivery_degraded {
        "incomplete · stream gap"
    } else if live {
        "recording"
    } else {
        match graph.integrity() {
            astra_turn_types::ExplainAnalyzeGraphIntegrityV1::Consistent
                if graph.nodes().iter().all(|node| node.terminal_observed) =>
            {
                if graph.coverage_gaps().is_empty() {
                    "recorded"
                } else {
                    "partial capture"
                }
            }
            astra_turn_types::ExplainAnalyzeGraphIntegrityV1::Unknown => "incomplete",
            astra_turn_types::ExplainAnalyzeGraphIntegrityV1::Consistent => "incomplete",
        }
    };
    let clock_labels = clock_labels(graph);
    let coverage_gaps = graph.coverage_gaps();
    let observation_incomplete = delivery_degraded || !coverage_gaps.is_empty();
    let peak = graph
        .max_concurrency()
        .map(|count| {
            if !observation_incomplete {
                count.to_string()
            } else {
                format!("≥{count}")
            }
        })
        .unwrap_or_else(|| "—".into());
    let parallel_label = if observation_incomplete {
        "observed overlap"
    } else if clock_labels.len() > 1 {
        "peak per clock"
    } else {
        "peak parallel"
    };
    let width = usize::from(width.max(1));
    let clock_count = if clock_labels.len() == 1 {
        "1 clock".to_string()
    } else {
        format!("{} clocks", clock_labels.len())
    };
    let header = format!(
        "Explain Analyze · {integrity} · {} stages · {parallel_label} {peak} · {clock_count}",
        graph.nodes().len(),
    );
    let mut lines = vec![Line::from(Span::styled(
        truncate(&header, width),
        Style::default().fg(theme.accent).bold(),
    ))];

    let content_limit = node_limit.map(|limit| limit.saturating_sub(1));
    let mut truncated = false;
    if !coverage_gaps.is_empty() && has_room(&lines, content_limit) {
        let coverage = format!(
            "Not timed separately · {}",
            coverage_gaps
                .iter()
                .map(|gap| gap.label())
                .collect::<Vec<_>>()
                .join(" · ")
        );
        if !push_wrapped_detail(
            &mut lines,
            &coverage,
            width,
            Style::default().fg(Color::Yellow),
            content_limit,
        ) {
            truncated = true;
        }
    }

    if let Some(summary) = provider_usage_summary(graph)
        && has_room(&lines, content_limit)
    {
        if !push_wrapped_detail(
            &mut lines,
            &summary,
            width,
            Style::default().fg(Color::DarkGray),
            content_limit,
        ) {
            truncated = true;
        }
    }

    let mut seen = HashSet::new();
    let mut stack = Vec::new();
    let mut roots = graph.roots().collect::<Vec<_>>();
    roots.sort_unstable();
    let mut root_cursor = 0;
    let mut orphan_heading = false;

    'tree: loop {
        if stack.is_empty() {
            while root_cursor < roots.len() && seen.contains(&roots[root_cursor]) {
                root_cursor += 1;
            }
            if root_cursor < roots.len() {
                let root = roots[root_cursor];
                let last = root_cursor + 1 == roots.len();
                root_cursor += 1;
                stack.push((root, Vec::new(), last));
            } else if let Some(orphan) =
                (0..graph.nodes().len()).find(|index| !seen.contains(index))
            {
                if !orphan_heading {
                    if !has_room(&lines, content_limit) {
                        truncated = true;
                        break;
                    }
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

        while let Some((index, ancestors_with_following_sibling, last)) = stack.pop() {
            if seen.contains(&index) {
                continue;
            }
            if !has_room(&lines, content_limit) {
                truncated = true;
                break 'tree;
            }
            seen.insert(index);
            let Some(node) = graph.nodes().get(index) else {
                continue;
            };

            let prefix = tree_prefix(&ancestors_with_following_sibling, last);
            let duration = node.duration_ms.map(format_ms).unwrap_or_else(|| {
                if node.terminal_observed {
                    "?".to_string()
                } else {
                    "…".to_string()
                }
            });
            let unresolved_terminal = graph.diagnostics().iter().any(|diagnostic| {
                diagnostic.code
                    == astra_turn_types::ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode
                    && diagnostic.node_id.as_deref() == Some(node.node_id.as_str())
            });
            let (state, state_color, state_short) =
                node_state(node, unresolved_terminal, live, theme);
            let offset = if clock_labels.len() > 1 {
                format!(
                    "{}+{}",
                    clock_labels[&node.clock_domain_id],
                    format_ms(node.start_elapsed_ms)
                )
            } else {
                format!("+{}", format_ms(node.start_elapsed_ms))
            };
            let (row, compact_row) =
                node_row(node, &prefix, &offset, &duration, state, state_short, width);
            let node_style = match node.kind {
                astra_turn_types::ExplainAnalyzeNodeKindV1::Wait => Style::default().fg(theme.warn),
                astra_turn_types::ExplainAnalyzeNodeKindV1::Admission => {
                    Style::default().fg(Color::DarkGray)
                }
                _ => Style::default().fg(theme.accent),
            };
            if compact_row {
                lines.push(Line::from(Span::styled(row, node_style)));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(row, node_style),
                    Span::styled(format!("  {state}"), Style::default().fg(state_color)),
                ]));
            }
            if compact_row {
                // The status is abbreviated in the narrow tree row; a second
                // row keeps the full state explicit for screen readers and
                // anyone using a narrow split terminal.
                if has_room(&lines, content_limit) {
                    lines.push(Line::from(Span::styled(
                        truncate(&format!("  State · {state}"), width),
                        Style::default().fg(state_color),
                    )));
                } else {
                    truncated = true;
                }
            }

            if let Some(usage) = &node.usage {
                let lanes = [
                    usage
                        .fresh_input_tokens
                        .map(|n| format!("in {}", format_tokens(n))),
                    usage
                        .cache_read_tokens
                        .map(|n| format!("cache read {}", format_tokens(n))),
                    usage
                        .cache_creation_tokens
                        .map(|n| format!("cache write {}", format_tokens(n))),
                    usage
                        .output_tokens
                        .map(|n| format!("out {}", format_tokens(n))),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                if !lanes.is_empty() {
                    let basis = match usage.basis {
                        astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact => {
                            "provider reported"
                        }
                        astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderPartial => {
                            "partial provider report"
                        }
                        astra_turn_types::ExplainAnalyzeUsageBasisV1::RuntimeEstimated => {
                            "runtime estimate"
                        }
                    };
                    if !push_wrapped_detail(
                        &mut lines,
                        &format!("{basis} · {lanes} tokens"),
                        width,
                        Style::default().fg(Color::DarkGray),
                        content_limit,
                    ) {
                        truncated = true;
                    }
                }
            }

            if let Some(context) = &node.context {
                if let Some(budget) = &context.budget {
                    let details = [
                        format!(
                            "input {} / limit {} · system {}",
                            format_tokens(budget.estimated_input_tokens),
                            format_tokens(budget.effective_input_limit_tokens),
                            format_tokens(budget.estimated_system_tokens),
                        ),
                        format!(
                            "tool schema {} · requested output {}",
                            format_tokens(budget.tool_schema_tokens),
                            format_tokens(budget.requested_output_tokens),
                        ),
                        format!(
                            "protocol reserve {} · model context {}",
                            format_tokens(budget.reserved_protocol_tokens),
                            format_tokens(budget.model_context_limit_tokens),
                        ),
                        format!("visible tools {}", budget.visible_tool_count),
                    ];
                    let heading_recorded = push_wrapped_detail(
                        &mut lines,
                        "Request budget · pre-provider estimate",
                        width,
                        Style::default().fg(Color::Cyan).bold(),
                        content_limit,
                    );
                    if !heading_recorded {
                        truncated = true;
                    } else {
                        for detail in details {
                            if !push_wrapped_detail(
                                &mut lines,
                                &detail,
                                width,
                                Style::default().fg(Color::Cyan),
                                content_limit,
                            ) {
                                truncated = true;
                                break;
                            }
                        }
                    }
                }
                if let Some(assembly) = &context.assembly {
                    if !push_wrapped_detail(
                        &mut lines,
                        "Context source estimates",
                        width,
                        Style::default().fg(Color::Cyan),
                        content_limit,
                    ) {
                        truncated = true;
                    } else {
                        for source in &assembly.sources {
                            if !push_wrapped_detail(
                                &mut lines,
                                &format!(
                                    "{} · {} tokens · {} sections",
                                    source_label(source.kind),
                                    format_tokens(source.estimated_tokens),
                                    source.section_count
                                ),
                                width,
                                Style::default().fg(Color::Cyan),
                                content_limit,
                            ) {
                                truncated = true;
                                break;
                            }
                        }
                    }
                }
            }

            let mut children = graph.children(index).to_vec();
            if children
                .iter()
                .all(|child| graph.nodes()[*child].clock_domain_id == node.clock_domain_id)
            {
                children.sort_by_key(|child| graph.nodes()[*child].start_elapsed_ms);
            }
            let mut child_ancestors = ancestors_with_following_sibling;
            child_ancestors.push(!last);
            for (position, child) in children.iter().enumerate().rev() {
                stack.push((
                    *child,
                    child_ancestors.clone(),
                    position + 1 == children.len(),
                ));
            }
            if truncated && !has_room(&lines, content_limit) {
                break 'tree;
            }
        }
    }

    if truncated {
        debug_assert!(node_limit.is_none_or(|limit| lines.len() < limit));
        let remaining = graph.nodes().len().saturating_sub(seen.len());
        let message = if remaining > 0 {
            format!("  · {remaining} more stages in turn history")
        } else {
            "  · More stage details in turn history".to_string()
        };
        lines.push(Line::from(Span::styled(
            truncate(&message, width),
            Style::default().fg(Color::DarkGray),
        )));
    }
    if !graph.diagnostics().is_empty() && !truncated && !live {
        let summary = graph
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic_label(diagnostic.code))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(3)
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(Line::from(Span::styled(
            truncate(
                &format!(
                    "⚠ Review measurements · {summary}{}",
                    if graph.diagnostics().len() > 3 {
                        " · more"
                    } else {
                        ""
                    }
                ),
                width,
            ),
            Style::default().fg(theme.warn),
        )));
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

fn has_room(lines: &[Line<'static>], content_limit: Option<usize>) -> bool {
    content_limit.is_none_or(|limit| lines.len() < limit)
}

/// Append a complete factual detail row, wrapping at word boundaries instead
/// of silently dropping the tail of context and token measurements.
fn push_wrapped_detail(
    lines: &mut Vec<Line<'static>>,
    detail: &str,
    width: usize,
    style: Style,
    content_limit: Option<usize>,
) -> bool {
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
        if !has_room(lines, content_limit) {
            return false;
        }
        let prefix = if index == 0 {
            first_prefix.as_str()
        } else {
            continuation_prefix.as_str()
        };
        lines.push(Line::from(Span::styled(format!("{prefix}{part}"), style)));
    }
    true
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

fn node_row(
    node: &astra_turn_types::ExplainAnalyzeProjectedNodeV1,
    prefix: &str,
    offset: &str,
    duration: &str,
    state: &str,
    state_short: &str,
    width: usize,
) -> (String, bool) {
    if width >= 54 {
        let fixed = prefix.width() + offset.width() + duration.width().max(7) + state.width() + 6;
        let label = truncate(&node.label, width.saturating_sub(fixed).max(1));
        return (format!("{prefix}{label}  {offset}  {duration:>7}"), false);
    }

    let compact_prefix = prefix
        .replace("│ ", "│")
        .replace("  ", " ")
        .replace("├─ ", "├")
        .replace("└─ ", "└");
    let time = if width >= 36 {
        format!("{offset} {duration}")
    } else {
        duration.to_owned()
    };
    let fixed = compact_prefix.width() + time.width() + state_short.width() + 2;
    let label = truncate(&node.label, width.saturating_sub(fixed).max(1));
    (
        truncate(
            &format!("{compact_prefix}{label} {time} {state_short}"),
            width,
        ),
        true,
    )
}

fn node_state(
    node: &astra_turn_types::ExplainAnalyzeProjectedNodeV1,
    unresolved_terminal: bool,
    live: bool,
    theme: &crate::tui::theme::Theme,
) -> (&'static str, Color, &'static str) {
    if node.conflicted {
        return ("Conflicting facts", theme.warn, "conflict");
    }
    if unresolved_terminal {
        return ("End not recorded", theme.warn, "open");
    }
    if !node.terminal_observed {
        return if live {
            ("Running", theme.accent, "run")
        } else {
            ("End not recorded", theme.dim, "open")
        };
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

fn provider_usage_summary(graph: &ExplainAnalyzeGraphV1) -> Option<String> {
    use astra_turn_types::{ExplainAnalyzeNodeKindV1::ProviderAttempt, ExplainAnalyzeUsageBasisV1};

    let attempts = graph
        .nodes()
        .iter()
        .filter(|node| node.kind == ProviderAttempt)
        .collect::<Vec<_>>();
    if attempts.is_empty() {
        return None;
    }

    let mut lanes: [(&str, u128, usize); 4] = [
        ("in", 0, 0),
        ("cache read", 0, 0),
        ("cache write", 0, 0),
        ("out", 0, 0),
    ];
    for node in &attempts {
        if !node.terminal_observed || node.conflicted {
            continue;
        }
        let Some(usage) = &node.usage else {
            continue;
        };
        if usage.basis == ExplainAnalyzeUsageBasisV1::RuntimeEstimated {
            continue;
        }
        for (lane, value) in lanes.iter_mut().zip([
            usage.fresh_input_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            usage.output_tokens,
        ]) {
            if let Some(value) = value {
                lane.1 += u128::from(value);
                lane.2 += 1;
            }
        }
    }
    let values = lanes
        .into_iter()
        .filter(|(_, _, count)| *count > 0)
        .map(|(label, total, count)| {
            let coverage = if count == attempts.len() {
                String::new()
            } else {
                format!(" {count}/{}", attempts.len())
            };
            format!("{label} {}{coverage}", format_tokens_u128(total))
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Some("Provider tokens · usage not reported yet".to_string());
    }
    Some(format!("Provider tokens · {}", values.join(" · ")))
}

fn format_tokens(value: u64) -> String {
    format_tokens_u128(u128::from(value))
}

fn format_tokens_u128(value: u128) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn truncate(value: &str, max: usize) -> String {
    if value.width() <= max {
        return value.to_owned();
    }
    let mut out = String::new();
    for ch in value.chars() {
        if out.width() + UnicodeWidthChar::width(ch).unwrap_or(0) + 1 > max {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
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
                assembly: Some(ExplainAnalyzeContextAssemblyV1 {
                    basis: ExplainAnalyzeContextAssemblyBasisV1::RuntimeTextEstimate,
                    sources: vec![ExplainAnalyzeContextSourceV1 {
                        kind: ExplainAnalyzeContextSourceKindV1::Memory,
                        section_count: 4,
                        estimated_tokens: 90,
                    }],
                }),
            }),
        ));
        graph.apply(finished(
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
        ));
        graph
    }

    #[test]
    fn live_tree_explains_time_usage_and_context_without_mixing_estimates() {
        let graph = example_graph();
        let rendered = text(&ExplainAnalyzeCell::live_lines(&graph, 120, 24, false));

        assert!(rendered.contains("recording"), "{rendered}");
        assert!(!rendered.contains("incomplete"), "{rendered}");
        assert!(rendered.contains("+1.2s"), "{rendered}");
        assert!(rendered.contains("420ms"), "{rendered}");
        assert!(rendered.contains("Failed"), "{rendered}");
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
            rendered.contains("Context source estimates\n")
                && rendered.contains("Memory · 90 tokens · 4 sections"),
            "{rendered}"
        );
    }

    #[test]
    fn live_empty_graph_labels_transport_gap_instead_of_waiting_silently() {
        let graph = ExplainAnalyzeGraphV1::default();
        let rendered = text(&ExplainAnalyzeCell::live_lines(&graph, 100, 4, true));
        assert!(rendered.contains("incomplete · stream gap"), "{rendered}");
    }

    #[test]
    fn wrapped_context_details_keep_every_measurement_readable_at_narrow_widths() {
        let graph = example_graph();
        let lines = ExplainAnalyzeCell::new(graph, false).display_lines(48);
        let rendered = text(&lines);

        for expected in [
            "input 123 / limit 1,000",
            "system 20",
            "tool schema 3",
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
    fn frozen_snapshot_does_not_invent_an_end_for_an_open_turn() {
        let mut graph = example_graph();
        graph.finish_ingest();
        let cell = ExplainAnalyzeCell::new(graph, false);
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

        let rendered = text(&ExplainAnalyzeCell::new(graph, false).display_lines(120));
        assert!(rendered.contains("partial capture"), "{rendered}");
        assert!(
            rendered.contains("Not timed separately · child-run timing · tool I/O wait breakdown"),
            "{rendered}"
        );
    }

    #[test]
    fn independent_clock_offsets_are_labeled_and_live_rows_stay_bounded() {
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
        let rendered = ExplainAnalyzeCell::live_lines(&graph, 80, 8, false);
        let output = text(&rendered);

        assert!(output.contains("2 clocks"), "{output}");
        assert!(output.contains("C1+0ms"), "{output}");
        assert!(output.contains("C2+500ms"), "{output}");
        assert!(rendered.len() <= 8, "{} rows", rendered.len());
        assert!(rendered.iter().all(|line| {
            line.spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
                <= 80
        }));
    }
}

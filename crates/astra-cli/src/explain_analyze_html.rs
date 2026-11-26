//! Standalone HTML Explain Analyze rendering over canonical execution facts.
//!
//! The HTML report is deliberately a derived, local view.  It contains no
//! scripts, network requests, user supplied DOM identifiers, or unbounded
//! recursive markup.  A bounded writer keeps malformed or unexpectedly large
//! graphs from producing a partial document.

use std::collections::{HashMap, HashSet};

use crate::explain_analyze_format::{
    alpha_label, compact_usage, decision_detail_line, diagnostic_label, format_tokens,
    provider_usage_summary,
};
#[cfg(test)]
use astra_turn_types::ExplainAnalyzeEventV1;
use astra_turn_types::{
    ExplainAnalyzeGraphV1, ExplainAnalyzeNodeKindV1, ExplainAnalyzeOutcomeV1,
    ExplainAnalyzeProjectedNodeV1, ExplainAnalyzeUsageBasisV1,
};

const MAX_HTML_BYTES: usize = 1024 * 1024;
const MAX_RENDER_NODES: usize = 2_048;
const MAX_DIAGNOSTICS: usize = 64;
const MAX_TEXT_CHARS: usize = 4_096;
const MAX_DEPENDENCIES: usize = 64;
const MAX_DEPENDENCY_LABEL_CHARS: usize = 512;
const MAX_TREE_DEPTH: usize = 256;
const TAIL_RESERVE_BYTES: usize = 4_096;

/// Render a complete, offline HTML document from the same events used by the
/// plain renderer.  The graph is reduced once and all traversal is iterative,
/// so a deep or cyclic graph remains safe to inspect.
#[cfg(test)]
pub(crate) fn render(
    events: &[ExplainAnalyzeEventV1],
    verbose: bool,
    delivery_degraded: bool,
) -> String {
    render_graph(
        &crate::explain_analyze_format::project(events),
        verbose,
        delivery_degraded,
    )
}

pub(crate) fn render_graph(
    graph: &ExplainAnalyzeGraphV1,
    verbose: bool,
    delivery_degraded: bool,
) -> String {
    let status = report_status(graph, delivery_degraded);
    let mut turns = graph
        .nodes()
        .iter()
        .filter(|node| node.kind == ExplainAnalyzeNodeKindV1::Turn);
    let first_turn = turns.next();
    let multiple_turns = turns.next().is_some();
    let primary = first_turn.filter(|node| !multiple_turns && !node.conflicted);
    let mut writer = HtmlWriter::new();
    writer.push(HTML_HEAD);
    writer.push(include_str!("explain_analyze_html.css"));
    writer.push("</style></head>");
    writer.push(&format!(
        concat!(
            "<body><main class=\"report\"><header class=\"masthead\">",
            "<div class=\"product\"><strong>Astra</strong><span>Explain Analyze</span></div>{}</header>"
        ),
        report_identity(graph),
    ));
    writer.push(&run_overview(
        graph,
        primary,
        multiple_turns,
        &status,
        delivery_degraded,
    ));
    if !graph.nodes().is_empty() {
        render_stage_list(&mut writer, graph, verbose);
    }

    let mut auxiliary_lines =
        crate::explain_analyze_report::auxiliary_usage_lines_with_detail(graph, verbose);
    auxiliary_lines.extend(crate::explain_analyze_report::auxiliary_details_lines(
        graph,
    ));
    if !auxiliary_lines.is_empty() {
        let mut section = String::from(
            "<details class=\"panel supplemental\"><summary>Auxiliary model usage and settlement</summary>",
        );
        for line in auxiliary_lines {
            section.push_str(&format!("<p>{}</p>", escape_html(&line, MAX_TEXT_CHARS)));
        }
        section.push_str("</details>");
        writer.push(&section);
    }

    if graph.nodes().is_empty() {
        let message = if delivery_degraded {
            "Runtime facts were not recovered before stream delivery stopped. The canonical JSON artifact remains the source of truth for replay."
        } else {
            "No runtime facts were captured for this turn. Run Explain Analyze again after the session has started."
        };
        writer.push(&format!(
            "<section class=\"panel\"><h2>No stages to display</h2><p>{}</p></section>",
            escape_html(message, MAX_TEXT_CHARS)
        ));
    } else {
        if delivery_degraded || !graph.coverage_gaps().is_empty() {
            writer.push(&coverage_card(graph, delivery_degraded));
        }
        if !graph.diagnostics().is_empty() {
            writer.push(&diagnostics_card(graph));
        }
    }

    writer.finish()
}

fn report_identity(graph: &ExplainAnalyzeGraphV1) -> String {
    let Some(first) = graph.nodes().first() else {
        return String::new();
    };
    if graph.nodes().iter().any(|node| node.run_id != first.run_id) {
        return "<p class=\"run-identity\">Multiple recorded runs</p>".to_string();
    }
    format!(
        "<p class=\"run-identity\">Run <code>{}</code></p>",
        escape_html(&first.run_id, MAX_TEXT_CHARS),
    )
}

const HTML_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<title>Astra Explain Analyze</title>
<style>
"#;

struct ReportStatus {
    label: &'static str,
    class: &'static str,
}

fn report_status(graph: &ExplainAnalyzeGraphV1, delivery_degraded: bool) -> ReportStatus {
    if graph.nodes().is_empty() {
        return ReportStatus {
            label: "no runtime facts",
            class: "empty",
        };
    }
    let all_terminal = graph.nodes().iter().all(|node| node.terminal_observed);
    let coverage = !graph.coverage_gaps().is_empty();
    if !delivery_degraded && graph.diagnostics().is_empty() && all_terminal && !coverage {
        ReportStatus {
            label: "recorded",
            class: "recorded",
        }
    } else if !delivery_degraded && all_terminal && graph.diagnostics().is_empty() {
        ReportStatus {
            label: "partial capture",
            class: "partial",
        }
    } else {
        ReportStatus {
            label: "incomplete",
            class: "incomplete",
        }
    }
}

fn run_overview(
    graph: &ExplainAnalyzeGraphV1,
    primary: Option<&ExplainAnalyzeProjectedNodeV1>,
    multiple_turns: bool,
    status: &ReportStatus,
    delivery_degraded: bool,
) -> String {
    let (result_class, result_label) = primary
        .map(|node| {
            (
                state_class(node_state(node.terminal_observed, node.outcome)),
                outcome_label(node.outcome),
            )
        })
        .unwrap_or((
            "incomplete",
            if multiple_turns {
                "multiple recorded turns"
            } else {
                "outcome unavailable"
            },
        ));
    let wall_time = primary
        .and_then(|node| node.duration_ms)
        .map(format_time)
        .unwrap_or_else(|| "—".to_string());
    let mut attempts = 0;
    let mut usage_reports = 0;
    let mut timed = 0;
    for node in graph.nodes().iter().filter(|node| !node.conflicted) {
        if node.kind == ExplainAnalyzeNodeKindV1::ProviderAttempt {
            attempts += 1;
            usage_reports += usize::from(node.usage.is_some());
        }
        timed += usize::from(node.terminal_observed && node.duration_ms.is_some());
    }
    let usage = if attempts == 0 {
        "—".to_string()
    } else {
        format!("{usage_reports}/{attempts}")
    };
    let overlap = graph
        .max_concurrency()
        .map(|overlap| {
            let lower_bound = if status.class == "recorded" {
                ""
            } else {
                "≥"
            };
            format!("{lower_bound}{overlap}")
        })
        .unwrap_or_else(|| "—".to_string());
    let (capture_class, capture_label) = if delivery_degraded {
        ("incomplete", "delivery interrupted")
    } else {
        (status.class, status.label)
    };
    let title = primary.map_or("Execution report", |node| node.label.as_str());
    format!(
        concat!(
            "<section class=\"run-overview\" aria-labelledby=\"run-title\">",
            "<div class=\"run-heading\"><h1 id=\"run-title\">{}</h1>",
            "<div class=\"run-status\"><span class=\"result result-{}\">{}</span>",
            "<span class=\"capture capture-{}\">{}</span></div></div>",
            "<div class=\"summary-strip\">",
            "<div class=\"summary-stat\"><span>Turn wall time</span><strong>{}</strong></div>",
            "<div class=\"summary-stat\"><span>Peak overlap / clock</span><strong>{}</strong></div>",
            "<div class=\"summary-stat\"><span>Timed stages</span><strong>{}/{}</strong></div>",
            "<div class=\"summary-stat\"><span>Attempts with usage</span><strong>{}</strong></div>",
            "</div></section>"
        ),
        escape_html(title, 180),
        result_class,
        result_label,
        capture_class,
        capture_label,
        wall_time,
        overlap,
        timed,
        graph.nodes().len(),
        usage,
    )
}

fn findings(graph: &ExplainAnalyzeGraphV1, rendered: &HashSet<usize>) -> String {
    let mut markup = String::new();
    if let Some((index, step)) = graph.longest_measured_step() {
        markup.push_str(&finding(
            rendered.contains(&index).then_some(index),
            "slow",
            "Longest measured step",
            &step.duration_ms.map(format_time).unwrap_or_default(),
            &step.label,
        ));
    }
    let failures = graph.nodes().iter().enumerate().filter(|(_, node)| {
        node.terminal_observed
            && !node.conflicted
            && matches!(
                node.outcome,
                Some(
                    ExplainAnalyzeOutcomeV1::Failed
                        | ExplainAnalyzeOutcomeV1::Interrupted
                        | ExplainAnalyzeOutcomeV1::Blocked
                        | ExplainAnalyzeOutcomeV1::Rejected
                )
            )
    });
    let mut evidence = None;
    let mut count = 0;
    for (index, failure) in failures {
        count += 1;
        if evidence.is_none_or(|(_, node): (usize, &ExplainAnalyzeProjectedNodeV1)| {
            node.decision_detail.is_none() && failure.decision_detail.is_some()
        }) {
            evidence = Some((index, failure));
        }
    }
    if let Some((index, failure)) = evidence {
        let detail = failure.decision_detail.as_ref().map_or_else(
            || format!("{} · {}", failure.label, outcome_label(failure.outcome)),
            |detail| format!("{} · {}", failure.label, decision_detail_line(detail)),
        );
        markup.push_str(&finding(
            rendered.contains(&index).then_some(index),
            "failure",
            if count == 1 {
                "Stage with an error"
            } else {
                "Stages with errors"
            },
            &count.to_string(),
            &detail,
        ));
    }
    if markup.is_empty() {
        markup
    } else {
        format!("<div class=\"findings\">{markup}</div>")
    }
}

fn finding(target: Option<usize>, class: &str, title: &str, value: &str, detail: &str) -> String {
    let body = format!(
        concat!(
            "<strong class=\"finding-value\">{}</strong>",
            "<span class=\"finding-content\"><span>{}</span><span>{}</span></span>"
        ),
        escape_html(value, 24),
        title,
        escape_html(detail, 200),
    );
    if let Some(index) = target {
        format!(
            "<a class=\"finding {class}\" href=\"#stage-{index}\">{body}<span class=\"finding-arrow\" aria-hidden=\"true\">↘</span></a>"
        )
    } else {
        format!("<div class=\"finding {class}\">{body}</div>")
    }
}

fn coverage_card(graph: &ExplainAnalyzeGraphV1, delivery_degraded: bool) -> String {
    let mut items = String::new();
    for gap in graph.coverage_gaps() {
        items.push_str("<li>");
        items.push_str(&escape_html(gap.label(), MAX_TEXT_CHARS));
        items.push_str("</li>");
    }
    if delivery_degraded {
        items.push_str("<li>stream delivery ended before all facts arrived</li>");
    }
    format!(
        "<details class=\"panel supplemental coverage\"><summary>Timing and capture gaps</summary><ul class=\"coverage-list\">{items}</ul></details>"
    )
}

fn diagnostics_card(graph: &ExplainAnalyzeGraphV1) -> String {
    let mut items = String::new();
    let mut count = 0;
    for diagnostic in graph.diagnostics().iter().take(MAX_DIAGNOSTICS) {
        count += 1;
        items.push_str("<li>");
        items.push_str(&escape_html(
            diagnostic_label(diagnostic.code),
            MAX_TEXT_CHARS,
        ));
        if diagnostic.node_id.is_some() {
            items.push_str(" · stage fact");
        }
        items.push_str("</li>");
    }
    let remaining = graph.diagnostics().len().saturating_sub(count);
    if remaining > 0 {
        items.push_str(&format!("<li>{remaining} additional observation gaps</li>"));
    }
    format!(
        "<details class=\"panel supplemental diagnostics\"><summary>Observation diagnostics</summary><ul class=\"diagnostic-list\">{items}</ul></details>"
    )
}

fn render_stage_list(writer: &mut HtmlWriter, graph: &ExplainAnalyzeGraphV1, verbose: bool) {
    let mut roots = graph.roots().collect::<Vec<_>>();
    roots.sort_unstable();
    let mut stack = roots
        .into_iter()
        .rev()
        .map(|index| (index, 0usize))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    let mut rendered = 0usize;
    let mut orphan_heading = false;
    let mut orphan_cursor = 0usize;
    let mut orphan_pending = false;
    let axes = clock_axes(graph);
    let multiple_clocks = axes.len() > 1;
    // The heading, linked findings and token summary are bounded separately
    // from the rows. Leave space for them before admitting each whole row.
    let rows_budget = writer.remaining_capacity().saturating_sub(6_144);
    let mut rows = String::with_capacity(rows_budget.min(16 * 1024));

    while rendered < MAX_RENDER_NODES {
        let Some((index, depth)) = stack.pop() else {
            while orphan_cursor < graph.nodes().len() && visited.contains(&orphan_cursor) {
                orphan_cursor += 1;
            }
            let Some(next) = (orphan_cursor < graph.nodes().len()).then_some(orphan_cursor) else {
                break;
            };
            orphan_cursor += 1;
            orphan_pending = true;
            stack.push((next, 0));
            continue;
        };
        if !visited.insert(index) {
            continue;
        }
        let Some(node) = graph.nodes().get(index) else {
            continue;
        };
        let axis = &axes[node.clock_domain_id.as_str()];
        let row = render_node(graph, node, index, depth, axis, multiple_clocks, verbose);
        let orphan = if orphan_pending {
            // The heading is only emitted for an actually rendered orphan;
            // keeping it in the same bounded fragment avoids a dangling label
            // when the byte cap is reached between rows.
            "<h3 class=\"orphan-heading\">Unlinked stages</h3>"
        } else {
            ""
        };
        let heading = if !orphan_heading && !orphan.is_empty() {
            orphan
        } else {
            ""
        };
        if rows
            .len()
            .saturating_add(heading.len())
            .saturating_add(row.len())
            > rows_budget
        {
            visited.remove(&index);
            break;
        }
        if !heading.is_empty() {
            rows.push_str(heading);
            orphan_heading = true;
        }
        rows.push_str(&row);
        orphan_pending = false;
        rendered += 1;

        let mut children = graph.children(index).to_vec();
        if children
            .iter()
            .all(|child| graph.nodes()[*child].clock_domain_id == node.clock_domain_id)
        {
            children.sort_by_key(|child| graph.nodes()[*child].start_elapsed_ms);
        }
        let child_depth = depth.saturating_add(1).min(MAX_TREE_DEPTH);
        for child in children.into_iter().rev() {
            stack.push((child, child_depth));
        }
    }

    let heading = format!(
        concat!(
            "<section class=\"stages\" id=\"execution-tree\">",
            "<div class=\"stages-heading\"><div><h2>Execution waterfall</h2>",
            "<p>Showing {} of {} stages{}</p></div>",
            "<div class=\"legend\"><span class=\"model\">Model</span>",
            "<span class=\"tool\">Tool</span><span class=\"failure\">Failure</span></div></div>",
            "{}<div class=\"stage-table-head stage-grid\"><span>Call chain</span>{}",
            "<span>Duration</span><span>Tokens</span><span>Outcome</span></div>"
        ),
        rendered,
        graph.nodes().len(),
        if multiple_clocks {
            " · each clock has its own scale"
        } else {
            ""
        },
        findings(graph, &visited),
        time_ruler(&axes),
    );
    if writer.push(&heading) {
        if writer.push(&rows) {
            writer.omit(graph.nodes().len().saturating_sub(rendered));
        } else {
            writer.omit(graph.nodes().len());
        }
        if let Some(usage) = provider_usage_summary(graph) {
            writer.push(&format!(
                "<p class=\"usage-total\">{}</p>",
                escape_html(&usage, MAX_TEXT_CHARS),
            ));
        }
        writer.push_reserved("</section>");
    } else {
        writer.omit(graph.nodes().len());
    }
}

struct ClockAxis {
    label: String,
    measured: Option<(u64, u64)>,
}

fn clock_axes(graph: &ExplainAnalyzeGraphV1) -> HashMap<&str, ClockAxis> {
    let mut axes = HashMap::<&str, ClockAxis>::new();
    for node in graph.nodes() {
        let ordinal = axes.len();
        let axis = axes
            .entry(node.clock_domain_id.as_str())
            .or_insert_with(|| ClockAxis {
                label: format!("clock {}", alpha_label(ordinal)),
                measured: None,
            });
        if let Some((start, end)) = timeline_interval(node) {
            axis.measured = Some(match axis.measured {
                Some((first, last)) => (first.min(start), last.max(end)),
                None => (start, end),
            });
        }
    }
    axes
}

fn time_ruler(axes: &HashMap<&str, ClockAxis>) -> String {
    if axes.len() != 1 {
        return "<span>Elapsed by clock</span>".to_string();
    }
    let Some((start, end)) = axes.values().next().and_then(|axis| axis.measured) else {
        return "<span>Elapsed</span>".to_string();
    };
    let span = end.saturating_sub(start);
    let intervals = if span >= 4 { 4 } else { usize::from(span > 0) };
    let mut ticks = String::from("<span class=\"time-ruler\" aria-label=\"Elapsed time\">");
    for index in 0..=intervals {
        let offset = if intervals == 0 {
            0
        } else {
            (u128::from(span) * index as u128 / intervals as u128) as u64
        };
        let percent = 100 * index / intervals.max(1);
        ticks.push_str(&format!(
            "<span style=\"--tick:{percent}%\">{}</span>",
            format_time(start + offset),
        ));
    }
    ticks.push_str("</span>");
    ticks
}

fn timeline_interval(node: &ExplainAnalyzeProjectedNodeV1) -> Option<(u64, u64)> {
    if node.conflicted {
        return None;
    }
    let end = node.end_elapsed_ms.or_else(|| {
        node.duration_ms
            .and_then(|duration| node.start_elapsed_ms.checked_add(duration))
    })?;
    (end >= node.start_elapsed_ms).then_some((node.start_elapsed_ms, end))
}

fn format_time(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        let fraction = ms % 1_000;
        let time = format!("{}.{fraction:03}", ms / 1_000);
        format!("{}s", time.trim_end_matches('0').trim_end_matches('.'))
    }
}

fn timing_cell(
    node: &ExplainAnalyzeProjectedNodeV1,
    axis: &ClockAxis,
    multiple_clocks: bool,
) -> String {
    let clock = if multiple_clocks {
        format!("<span class=\"clock-label\">{}</span>", axis.label)
    } else {
        String::new()
    };
    let (Some((axis_start, axis_end)), Some((start, end))) =
        (axis.measured, timeline_interval(node))
    else {
        return format!(
            "<span class=\"stage-timing\">{clock}<span class=\"stage-track unmeasured\">{}</span></span>",
            if node.conflicted {
                "Conflicting timing"
            } else {
                "End unavailable"
            },
        );
    };
    let span = u128::from((axis_end - axis_start).max(1));
    let position = u128::from(start - axis_start) * 10_000 / span;
    let width = u128::from(end - start) * 10_000 / span;
    let (marker, qualifier) = if start == end {
        (" instant", " · instant")
    } else if width == 0 {
        (" instant micro", " · short interval")
    } else {
        ("", "")
    };
    let window = format!("{} → {}{qualifier}", format_time(start), format_time(end),);
    format!(
        concat!(
            "<span class=\"stage-timing\">{}<span class=\"stage-track\">",
            "<span class=\"interval{}\" style=\"--start:{:.2}%;--span:{:.2}%\" aria-hidden=\"true\"></span>",
            "</span><span class=\"stage-window\" aria-hidden=\"true\">{}</span></span>"
        ),
        clock,
        marker,
        position as f64 / 100.0,
        width as f64 / 100.0,
        window,
    )
}

fn render_node(
    graph: &ExplainAnalyzeGraphV1,
    node: &ExplainAnalyzeProjectedNodeV1,
    index: usize,
    depth: usize,
    axis: &ClockAxis,
    multiple_clocks: bool,
    verbose: bool,
) -> String {
    let state = node_state(node.terminal_observed, node.outcome);
    let state_class = state_class(state);
    let outcome = if node.conflicted {
        "conflicting facts"
    } else if node.terminal_observed {
        outcome_label(node.outcome)
    } else {
        "incomplete"
    };
    let duration = node
        .duration_ms
        .filter(|_| !node.conflicted)
        .map(format_time);
    let round = node
        .round_index
        .map(|round| format!("round {}", u64::from(round).saturating_add(1)));
    let attempt = node
        .attempt_index
        .map(|attempt| format!("attempt {}", u64::from(attempt).saturating_add(1)));
    let mut sequence = Vec::new();
    if let Some(round) = round {
        sequence.push(round);
    }
    if let Some(attempt) = attempt {
        sequence.push(attempt);
    }

    let mut body = format!(
        concat!(
            "<div class=\"full-label\">{}</div><dl class=\"stage-facts\">",
            "<div><dt>Kind</dt><dd>{}</dd></div><div><dt>Start</dt><dd>{}</dd></div>",
            "<div><dt>End</dt><dd>{}</dd></div><div><dt>Clock</dt><dd>{}</dd></div>",
            "<div><dt>Stage</dt><dd>{}</dd></div></dl>"
        ),
        escape_html(&node.label, MAX_TEXT_CHARS),
        kind_label(node.kind),
        format_time(node.start_elapsed_ms),
        timeline_interval(node)
            .map_or_else(|| "unavailable".to_string(), |(_, end)| format_time(end)),
        axis.label,
        index,
    );
    if let Some(detail) = &node.decision_detail {
        body.push_str(&format!(
            "<p class=\"detail-line\">{}</p>",
            escape_html(&decision_detail_line(detail), MAX_TEXT_CHARS)
        ));
    } else if node.outcome == Some(ExplainAnalyzeOutcomeV1::Failed) {
        body.push_str("<p class=\"detail-line\">Failure reason was not recorded.</p>");
    }
    if node.conflicted {
        body.push_str("<p class=\"detail-line\"><span class=\"chip warning-chip\">conflicting facts were retained for review</span></p>");
    }
    if let Some(usage) = &node.usage {
        body.push_str(&format!(
            "<p class=\"detail-line\"><strong>Token usage:</strong> {}</p>",
            escape_html(&usage_summary(usage), MAX_TEXT_CHARS)
        ));
    }
    if !sequence.is_empty() {
        body.push_str(&format!(
            "<p class=\"detail-line sequence\"><strong>Sequence:</strong> {}</p>",
            escape_html(&sequence.join(" · "), MAX_TEXT_CHARS)
        ));
    }
    if !node.coverage_gaps.is_empty() {
        body.push_str("<div class=\"chips\">");
        for gap in &node.coverage_gaps {
            body.push_str(&format!(
                "<span class=\"chip warning-chip\">{}</span>",
                escape_html(gap.label(), MAX_TEXT_CHARS)
            ));
        }
        body.push_str("</div>");
    }
    if !node.dependency_indices.is_empty() {
        body.push_str(
            "<p class=\"detail-line\"><strong>Dependencies:</strong></p><div class=\"chips\">",
        );
        for (position, dependency_index) in node
            .dependency_indices
            .iter()
            .enumerate()
            .take(MAX_DEPENDENCIES)
        {
            let label = dependency_index
                .and_then(|dependency| graph.nodes().get(dependency))
                .map(|dependency| dependency.label.as_str())
                .unwrap_or("unrecorded stage");
            body.push_str(&format!(
                "<span class=\"chip\">{} · {}</span>",
                position + 1,
                escape_html(label, MAX_DEPENDENCY_LABEL_CHARS)
            ));
        }
        if node.dependency_indices.len() > MAX_DEPENDENCIES {
            body.push_str(&format!(
                "<span class=\"chip warning-chip\">{} additional dependencies omitted</span>",
                node.dependency_indices.len() - MAX_DEPENDENCIES
            ));
        }
        body.push_str("</div>");
    }
    if verbose {
        if let Some(context) = &node.context {
            body.push_str(&context_markup(context));
        }
    }

    let parent = match (node.parent_index, node.parent_node_id.is_some()) {
        (Some(parent), _) => format!("stage {parent}"),
        (None, true) => "unresolved parent".to_string(),
        (None, false) => "root".to_string(),
    };
    body.push_str(&format!(
        "<p class=\"detail-line\"><strong>Parent:</strong> {} · <strong>Depth:</strong> {depth}</p>",
        escape_html(&parent, MAX_TEXT_CHARS)
    ));

    let indent = depth.min(8);
    let kind_class = match node.kind {
        ExplainAnalyzeNodeKindV1::ModelRound | ExplainAnalyzeNodeKindV1::ProviderAttempt => "model",
        ExplainAnalyzeNodeKindV1::ToolBatch | ExplainAnalyzeNodeKindV1::ToolCall => "tool",
        _ => "runtime",
    };
    let label = escape_html(&node.label, MAX_TEXT_CHARS);
    format!(
        concat!(
            "<details class=\"stage stage-row {} state-{}{}\" id=\"stage-{}\" style=\"--depth:{}\" data-index=\"{}\">",
            "<summary class=\"stage-grid\"><span class=\"stage-label\">",
            "<span class=\"stage-toggle\" aria-hidden=\"true\">›</span>",
            "<span class=\"stage-marker\" aria-hidden=\"true\"></span>",
            "<span class=\"stage-name\" title=\"{}\">{}</span></span>{}",
            "<span class=\"stage-duration\">{}</span><span class=\"stage-tokens\">{}</span>",
            "<span class=\"stage-outcome\">{}</span></summary><div class=\"stage-body\">{}</div></details>"
        ),
        kind_class,
        state_class,
        match (node.conflicted, depth > 0) {
            (true, true) => " conflict nested",
            (true, false) => " conflict",
            (false, true) => " nested",
            (false, false) => "",
        },
        index,
        indent,
        index,
        escape_html(&node.label, 160),
        label,
        timing_cell(node, axis, multiple_clocks),
        duration.as_deref().unwrap_or("—"),
        escape_html(
            &node
                .usage
                .as_ref()
                .map_or_else(|| "—".to_string(), compact_usage),
            MAX_TEXT_CHARS,
        ),
        outcome,
        body,
    )
}

fn context_markup(context: &astra_turn_types::ExplainAnalyzeContextMetricsV1) -> String {
    let mut markup = String::new();
    if let Some(budget) = &context.budget {
        markup.push_str(&format!(
            "<p class=\"detail-line\"><strong>Request estimate:</strong> {} input · {} limit · {} output reserved · {} visible tools</p>",
            format_tokens(budget.estimated_input_tokens),
            format_tokens(budget.effective_input_limit_tokens),
            format_tokens(budget.requested_output_tokens),
            budget.visible_tool_count,
        ));
    }
    if let Some(assembly) = &context.assembly {
        markup.push_str(
            "<p class=\"detail-line\"><strong>Context sources:</strong></p><div class=\"chips\">",
        );
        for source in &assembly.sources {
            markup.push_str(&format!(
                "<span class=\"chip\">{} · {} tokens · {} sections</span>",
                escape_html(source_label(source.kind), MAX_TEXT_CHARS),
                format_tokens(source.estimated_tokens),
                source.section_count
            ));
        }
        markup.push_str("</div>");
    }
    markup
}

fn usage_summary(usage: &astra_turn_types::ExplainAnalyzeTokenUsageV1) -> String {
    let basis = match usage.basis {
        ExplainAnalyzeUsageBasisV1::ProviderExact => "provider reported",
        ExplainAnalyzeUsageBasisV1::ProviderPartial => "partial provider report",
        ExplainAnalyzeUsageBasisV1::RuntimeEstimated => "runtime estimate",
    };
    let mut lanes = Vec::new();
    if let Some(value) = usage.fresh_input_tokens {
        lanes.push(format!("input {}", format_tokens(value)));
    }
    if let Some(value) = usage.cache_read_tokens {
        lanes.push(format!("cache read {}", format_tokens(value)));
    }
    if let Some(value) = usage.cache_creation_tokens {
        lanes.push(format!("cache write {}", format_tokens(value)));
    }
    if let Some(value) = usage.output_tokens {
        lanes.push(format!("output {}", format_tokens(value)));
    }
    format!("{basis} · {}", lanes.join(" · "))
}

fn kind_label(kind: ExplainAnalyzeNodeKindV1) -> &'static str {
    match kind {
        ExplainAnalyzeNodeKindV1::Run => "run",
        ExplainAnalyzeNodeKindV1::Turn => "turn",
        ExplainAnalyzeNodeKindV1::Admission => "admission",
        ExplainAnalyzeNodeKindV1::Preparation => "preparation",
        ExplainAnalyzeNodeKindV1::ContextAssembly => "context assembly",
        ExplainAnalyzeNodeKindV1::Judgment => "judgment",
        ExplainAnalyzeNodeKindV1::ModelRound => "model round",
        ExplainAnalyzeNodeKindV1::ProviderAttempt => "provider attempt",
        ExplainAnalyzeNodeKindV1::ToolBatch => "tool batch",
        ExplainAnalyzeNodeKindV1::ToolCall => "tool call",
        ExplainAnalyzeNodeKindV1::Wait => "wait",
        ExplainAnalyzeNodeKindV1::ChildRun => "child run",
        ExplainAnalyzeNodeKindV1::Settlement => "settlement",
    }
}

fn outcome_label(outcome: Option<ExplainAnalyzeOutcomeV1>) -> &'static str {
    match outcome {
        Some(ExplainAnalyzeOutcomeV1::Completed) => "completed",
        Some(ExplainAnalyzeOutcomeV1::Succeeded) => "succeeded",
        Some(ExplainAnalyzeOutcomeV1::Failed) => "failed",
        Some(ExplainAnalyzeOutcomeV1::Cancelled) => "cancelled",
        Some(ExplainAnalyzeOutcomeV1::Interrupted) => "interrupted",
        Some(ExplainAnalyzeOutcomeV1::Blocked) => "blocked",
        Some(ExplainAnalyzeOutcomeV1::Waiting) => "waiting",
        Some(ExplainAnalyzeOutcomeV1::Rejected) => "rejected",
        Some(ExplainAnalyzeOutcomeV1::Reused) => "reused",
        Some(ExplainAnalyzeOutcomeV1::Suppressed) => "suppressed",
        Some(ExplainAnalyzeOutcomeV1::Deferred) => "deferred",
        Some(ExplainAnalyzeOutcomeV1::Resolved) => "resolved",
        Some(ExplainAnalyzeOutcomeV1::Fallback) => "fallback",
        Some(ExplainAnalyzeOutcomeV1::Unavailable) => "unavailable",
        Some(ExplainAnalyzeOutcomeV1::Delegated) => "delegated",
        None => "outcome not recorded",
    }
}

fn node_state(terminal: bool, outcome: Option<ExplainAnalyzeOutcomeV1>) -> &'static str {
    if !terminal {
        return "incomplete";
    }
    match outcome {
        Some(
            ExplainAnalyzeOutcomeV1::Completed
            | ExplainAnalyzeOutcomeV1::Succeeded
            | ExplainAnalyzeOutcomeV1::Resolved,
        ) => "completed",
        Some(
            ExplainAnalyzeOutcomeV1::Failed
            | ExplainAnalyzeOutcomeV1::Rejected
            | ExplainAnalyzeOutcomeV1::Interrupted,
        ) => "failed",
        Some(
            ExplainAnalyzeOutcomeV1::Blocked
            | ExplainAnalyzeOutcomeV1::Waiting
            | ExplainAnalyzeOutcomeV1::Deferred,
        ) => "waiting",
        Some(ExplainAnalyzeOutcomeV1::Cancelled) => "cancelled",
        _ => "incomplete",
    }
}

fn state_class(state: &str) -> &'static str {
    match state {
        "completed" => "completed",
        "failed" => "failed",
        "waiting" => "waiting",
        "cancelled" => "cancelled",
        _ => "incomplete",
    }
}

fn source_label(kind: astra_turn_types::ExplainAnalyzeContextSourceKindV1) -> &'static str {
    use astra_turn_types::ExplainAnalyzeContextSourceKindV1::*;
    match kind {
        Identity => "identity",
        SelfModel => "self model",
        ProjectContext => "project context",
        DeferredTools => "deferred tools",
        AvailableSkills => "available skills",
        Memory => "memory",
        WorkingMemory => "working memory",
        History => "conversation history",
        Constraints => "constraints",
        Skills => "skills",
        RuntimeIdentity => "runtime identity",
        RuntimeVolatile => "runtime state",
    }
}

fn escape_html(value: &str, max_chars: usize) -> String {
    let mut escaped = String::new();
    let mut truncated = false;
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            truncated = true;
            break;
        }
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            ch if ch.is_control() => escaped.push('�'),
            ch => escaped.push(ch),
        }
    }
    if truncated {
        escaped.push('…');
    }
    escaped
}

struct HtmlWriter {
    output: String,
    omitted: usize,
}

impl HtmlWriter {
    fn new() -> Self {
        Self {
            output: String::with_capacity(16 * 1024),
            omitted: 0,
        }
    }

    fn push(&mut self, fragment: &str) -> bool {
        if self.output.len().saturating_add(fragment.len())
            <= MAX_HTML_BYTES.saturating_sub(TAIL_RESERVE_BYTES)
        {
            self.output.push_str(fragment);
            true
        } else {
            self.omitted = self.omitted.saturating_add(1);
            false
        }
    }

    fn remaining_capacity(&self) -> usize {
        MAX_HTML_BYTES
            .saturating_sub(TAIL_RESERVE_BYTES)
            .saturating_sub(self.output.len())
    }

    /// Closing markup is written from the reserved tail.  Keeping this path
    /// separate makes it impossible for a saturated content fragment to leave
    /// an open section behind.
    fn push_reserved(&mut self, fragment: &str) {
        debug_assert!(
            self.output.len().saturating_add(fragment.len()) <= MAX_HTML_BYTES,
            "reserved HTML tail exceeded the report bound"
        );
        self.output.push_str(fragment);
    }

    fn omit(&mut self, count: usize) {
        self.omitted = self.omitted.saturating_add(count);
    }

    fn finish(mut self) -> String {
        if self.omitted > 0 {
            let message = format!(
                "<section class=\"panel alert-panel coverage\"><h2>Offline report limit</h2><p class=\"note\">{} stage detail{} omitted to keep this offline report readable and below 1 MiB. The canonical JSON artifact retains the complete fact stream.</p></section>",
                self.omitted,
                if self.omitted == 1 { " was" } else { "s were" }
            );
            if self.output.len().saturating_add(message.len() + 256) <= MAX_HTML_BYTES {
                self.output.push_str(&message);
            }
        }
        self.output.push_str(
            "<footer class=\"footer\">Astra · Recorded runtime facts</footer></main></body></html>",
        );
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        EXPLAIN_ANALYZE_SCHEMA_VERSION, ExplainAnalyzeCoverageGapV1, ExplainAnalyzeTokenUsageV1,
        ExplainAnalyzeTransitionV1,
    };

    #[allow(clippy::too_many_arguments)]
    fn event(
        event_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        label: &str,
        transition: ExplainAnalyzeTransitionV1,
        start: u64,
        duration: Option<u64>,
        outcome: Option<ExplainAnalyzeOutcomeV1>,
    ) -> ExplainAnalyzeEventV1 {
        ExplainAnalyzeEventV1 {
            auxiliary_usage: None,
            auxiliary_details: None,
            schema_version: EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: event_id.to_string(),
            run_id: "run-1".to_string(),
            turn_id: "turn-1".to_string(),
            node_id: node_id.to_string(),
            parent_node_id: parent_node_id.map(str::to_string),
            dependency_node_ids: Vec::new(),
            producer_id: "runtime".to_string(),
            clock_domain_id: "clock-1".to_string(),
            kind: if node_id == "turn" {
                ExplainAnalyzeNodeKindV1::Turn
            } else {
                ExplainAnalyzeNodeKindV1::ToolCall
            },
            round_index: None,
            attempt_index: None,
            label: label.to_string(),
            transition,
            elapsed_ms: start + duration.unwrap_or_default(),
            start_elapsed_ms: (transition == ExplainAnalyzeTransitionV1::Finished).then_some(start),
            duration_ms: duration,
            outcome,
            decision_detail: None,
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        }
    }

    fn finished(event: ExplainAnalyzeEventV1, duration: u64) -> ExplainAnalyzeEventV1 {
        let mut finished = event;
        finished.event_id.push_str("-finished");
        finished.transition = ExplainAnalyzeTransitionV1::Finished;
        finished.elapsed_ms = finished.start_elapsed_ms.unwrap_or(finished.elapsed_ms) + duration;
        finished.start_elapsed_ms = Some(finished.elapsed_ms - duration);
        finished.duration_ms = Some(duration);
        finished.outcome = Some(ExplainAnalyzeOutcomeV1::Succeeded);
        finished
    }

    #[test]
    fn workspace_layout_separates_outcome_from_capture_and_uses_numeric_stage_refs() {
        let turn = finished(
            event(
                "event-turn",
                "turn",
                None,
                "Build a resilient handoff report",
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            ),
            5_600,
        );
        let mut child = finished(
            event(
                "event-child",
                "child",
                Some("turn"),
                "Provider attempt · retry 2",
                ExplainAnalyzeTransitionV1::Started,
                1_200,
                None,
                None,
            ),
            1_900,
        );
        child.kind = ExplainAnalyzeNodeKindV1::ProviderAttempt;
        child.round_index = Some(0);
        child.attempt_index = Some(1);
        let html = render(&[turn, child], false, false);
        assert!(html.contains("class=\"summary-strip\""));
        assert!(html.contains("class=\"capture capture-"));
        assert!(html.contains("Execution waterfall"));
        assert!(html.contains("Showing 2 of 2 stages"));
        assert!(html.contains("data-index=\"0\""));
        assert!(html.contains("href=\"#stage-1\""));
        assert!(html.contains("--start:21.42%;--span:33.92%"));
        assert!(html.contains("1.2s → 3.1s"));
        assert!(!html.contains("clock-1"));
    }

    #[test]
    fn missing_turn_does_not_get_promoted_to_task_outcome_or_wall_time() {
        let mut tool = finished(
            event(
                "event-tool",
                "tool",
                None,
                "Successful tool only",
                ExplainAnalyzeTransitionV1::Started,
                40,
                None,
                None,
            ),
            120,
        );
        tool.kind = ExplainAnalyzeNodeKindV1::ToolCall;
        let html = render(&[tool], false, false);
        assert!(html.contains("outcome unavailable"));
        assert!(html.contains("<span>Turn wall time</span><strong>—</strong>"));
    }

    #[test]
    fn zero_and_subpixel_intervals_remain_explicit_without_invented_width() {
        let instant = finished(
            event(
                "event-instant",
                "instant",
                None,
                "Instant event",
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            ),
            0,
        );
        let short = finished(
            event(
                "event-short",
                "short",
                None,
                "Very short event",
                ExplainAnalyzeTransitionV1::Started,
                10_000,
                None,
                None,
            ),
            1,
        );
        let html = render(&[instant, short], false, false);
        assert!(html.contains("0ms → 0ms · instant"));
        assert!(html.contains("class=\"interval instant micro\""));
        assert!(html.contains("10s → 10.001s · short interval"));
        assert!(!html.contains("--span:1.00%"));
    }

    #[test]
    fn row_tokens_identify_estimated_and_partial_usage() {
        let mut estimated = finished(
            event(
                "event-estimated",
                "estimated",
                None,
                "Estimated usage",
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            ),
            5,
        );
        estimated.kind = ExplainAnalyzeNodeKindV1::ProviderAttempt;
        estimated.round_index = Some(0);
        estimated.attempt_index = Some(0);
        estimated.usage = Some(ExplainAnalyzeTokenUsageV1 {
            basis: ExplainAnalyzeUsageBasisV1::RuntimeEstimated,
            fresh_input_tokens: Some(1_000),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            output_tokens: Some(10),
        });
        let html = render(&[estimated], false, false);
        assert!(html.contains("~in 1.00k / out 10"));
    }

    #[test]
    fn empty_report_is_standalone_and_explains_missing_facts() {
        let html = render(&[], false, true);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.ends_with("</html>"));
        assert!(html.contains("stream delivery stopped"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn hostile_labels_are_escaped_without_script_or_attribute_injection() {
        let started = event(
            "event-1",
            "turn",
            None,
            "</div><script>alert(1)</script>\"&",
            ExplainAnalyzeTransitionV1::Started,
            0,
            None,
            None,
        );
        let finished = finished(started, 10);
        let html = render(&[finished], false, false);
        assert!(html.contains("&lt;/div&gt;&lt;script&gt;"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("</div><script"));
    }

    #[test]
    fn deep_graph_is_iterative_and_bounded_with_valid_closing_tags() {
        let mut events = Vec::with_capacity(10_000);
        let mut parent = None;
        for index in 0..10_000 {
            let node = format!("node-{index}");
            let started = event(
                &format!("event-{index}"),
                &node,
                parent.as_deref(),
                &format!("Stage {index}"),
                ExplainAnalyzeTransitionV1::Started,
                index as u64,
                None,
                None,
            );
            events.push(finished(started, 1));
            parent = Some(node);
        }
        let html = render(&events, true, false);
        assert!(html.len() <= MAX_HTML_BYTES);
        assert!(html.ends_with("</html>"));
        assert!(html.contains("omitted to keep this offline report"));
        assert!(html.contains("Showing "));
        assert!(!html.contains("Showing 10000 of 10000 stages"));
    }

    #[test]
    fn coverage_and_diagnostics_are_actionable() {
        let mut turn = finished(
            event(
                "event-turn",
                "turn",
                None,
                "User turn",
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            ),
            12,
        );
        turn.coverage_gaps = vec![ExplainAnalyzeCoverageGapV1::FirstTokenLatency];
        let child = event(
            "event-child",
            "child",
            Some("missing-parent"),
            "Child",
            ExplainAnalyzeTransitionV1::Finished,
            1,
            Some(1),
            Some(ExplainAnalyzeOutcomeV1::Failed),
        );
        let html = render(&[turn, child], false, false);
        assert!(html.contains("time to first token"));
        assert!(html.contains("parent stage was not observed"));
    }

    #[test]
    fn overlap_bars_are_normalized_within_each_clock_domain() {
        let mut first = finished(
            event(
                "event-first",
                "first",
                None,
                "First clock",
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            ),
            100,
        );
        first.clock_domain_id = "clock-first".to_string();
        let mut second = finished(
            event(
                "event-second",
                "second",
                None,
                "Second clock",
                ExplainAnalyzeTransitionV1::Started,
                1_000,
                None,
                None,
            ),
            10,
        );
        second.clock_domain_id = "clock-second".to_string();
        let mut longer_second = finished(
            event(
                "event-longer-second",
                "longer-second",
                None,
                "Longer second clock",
                ExplainAnalyzeTransitionV1::Started,
                1_020,
                None,
                None,
            ),
            20,
        );
        longer_second.clock_domain_id = "clock-second".to_string();
        let html = render(&[first, second, longer_second], false, false);
        assert!(html.contains("each clock has its own scale"));
        assert!(html.contains("Elapsed by clock"));
        assert!(!html.contains("<span class=\"time-ruler\""));
        assert!(html.contains("--start:0.00%;--span:100.00%"));
        assert!(html.contains("--start:0.00%;--span:25.00%"));
        assert!(html.contains("--start:50.00%;--span:50.00%"));
    }

    #[test]
    fn high_fanout_dependencies_are_capped_and_explicitly_summarized() {
        let mut events = Vec::with_capacity(257);
        for index in 0..256 {
            let node = format!("dependency-{index}");
            events.push(finished(
                event(
                    &format!("event-{index}"),
                    &node,
                    None,
                    &format!("Dependency {index}"),
                    ExplainAnalyzeTransitionV1::Started,
                    index as u64,
                    None,
                    None,
                ),
                1,
            ));
        }
        let mut root = finished(
            event(
                "event-root",
                "turn",
                None,
                "High fanout",
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            ),
            1,
        );
        root.dependency_node_ids = (0..256)
            .map(|index| format!("dependency-{index}"))
            .collect();
        events.push(root);
        let html = render(&events, false, false);
        assert!(html.contains("192 additional dependencies omitted"));
        assert!(html.len() <= MAX_HTML_BYTES);
    }

    #[test]
    fn parent_stage_index_survives_deep_flat_rendering() {
        let parent = finished(
            event(
                "event-parent",
                "parent",
                None,
                "Parent",
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            ),
            2,
        );
        let child = finished(
            event(
                "event-child",
                "child",
                Some("parent"),
                "Child",
                ExplainAnalyzeTransitionV1::Started,
                1,
                None,
                None,
            ),
            1,
        );
        let html = render(&[parent, child], false, false);
        assert!(html.contains("<strong>Parent:</strong> stage 0"));
        assert!(html.contains("<strong>Depth:</strong> 1"));
    }

    #[test]
    fn maximum_round_and_attempt_values_do_not_overflow() {
        let mut model_round = finished(
            event(
                "event-round",
                "round",
                None,
                "Round",
                ExplainAnalyzeTransitionV1::Started,
                0,
                None,
                None,
            ),
            1,
        );
        model_round.kind = ExplainAnalyzeNodeKindV1::ModelRound;
        model_round.round_index = Some(u32::MAX);
        let mut attempt = finished(
            event(
                "event-attempt",
                "attempt",
                None,
                "Attempt",
                ExplainAnalyzeTransitionV1::Started,
                1,
                None,
                None,
            ),
            1,
        );
        attempt.kind = ExplainAnalyzeNodeKindV1::ProviderAttempt;
        attempt.round_index = Some(u32::MAX);
        attempt.attempt_index = Some(u32::MAX);
        let html = render(&[model_round, attempt], false, false);
        assert!(html.contains("round 4294967296"));
        assert!(html.contains("attempt 4294967296"));
    }

    #[test]
    fn bounded_document_keeps_all_container_tags_balanced() {
        let mut events = Vec::with_capacity(MAX_RENDER_NODES + 128);
        for index in 0..(MAX_RENDER_NODES + 128) {
            let node = format!("node-{index}");
            let label = format!("Stage {index} {}", "x".repeat(140));
            events.push(finished(
                event(
                    &format!("event-{index}"),
                    &node,
                    None,
                    &label,
                    ExplainAnalyzeTransitionV1::Started,
                    index as u64,
                    None,
                    None,
                ),
                1,
            ));
        }
        let html = render(&events, false, false);
        assert!(html.len() <= MAX_HTML_BYTES);
        assert_eq!(
            html.matches("<section").count(),
            html.matches("</section>").count()
        );
        assert_eq!(
            html.matches("<details").count(),
            html.matches("</details>").count()
        );
        assert!(html.ends_with("</html>"));
    }
}

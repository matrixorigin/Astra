//! Standalone HTML Explain Analyze rendering over canonical execution facts.
//!
//! The HTML report is deliberately a derived, local view.  It contains no
//! scripts, network requests, user supplied DOM identifiers, or unbounded
//! recursive markup.  A bounded writer keeps malformed or unexpectedly large
//! graphs from producing a partial document.

use std::collections::{BTreeSet, HashMap, HashSet};

use astra_turn_types::{
    ExplainAnalyzeEventV1, ExplainAnalyzeGraphV1, ExplainAnalyzeNodeKindV1,
    ExplainAnalyzeOutcomeV1, ExplainAnalyzeProjectedNodeV1,
    ExplainAnalyzeProjectionDiagnosticCodeV1, ExplainAnalyzeUsageBasisV1,
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
pub(crate) fn render(
    events: &[ExplainAnalyzeEventV1],
    verbose: bool,
    delivery_degraded: bool,
) -> String {
    let mut graph = ExplainAnalyzeGraphV1::default();
    for event in events {
        graph.apply(event.clone());
    }
    graph.finish_ingest();

    let status = report_status(&graph, delivery_degraded);
    let mut writer = HtmlWriter::new();
    writer.push(HTML_HEAD);
    writer.push(&format!(
        "<body><main class=\"report\"><header class=\"hero\"><div class=\"eyebrow\">ASTRA · EXPLAIN ANALYZE</div><h1>Execution report</h1><p class=\"lede\">A local, replayable view of the runtime facts captured for this turn.</p><div class=\"status-row\"><span class=\"status status-{}\">{}</span>{}</div></header>",
        status.class,
        escape_html(status.label, MAX_TEXT_CHARS),
        if delivery_degraded {
            "<span class=\"status status-warning\">stream delivery interrupted</span>"
        } else {
            ""
        },
    ));

    writer.push(&overview_card(&graph, delivery_degraded));

    if graph.nodes().is_empty() {
        let message = if delivery_degraded {
            "Runtime facts were not recovered before stream delivery stopped. The canonical JSON artifact remains the source of truth for replay."
        } else {
            "No runtime facts were captured for this turn. Run Explain Analyze again after the session has started."
        };
        writer.push(&format!(
            "<section class=\"empty card\"><div class=\"empty-icon\" aria-hidden=\"true\">∅</div><h2>No stages to display</h2><p>{}</p></section>",
            escape_html(message, MAX_TEXT_CHARS)
        ));
    } else {
        if delivery_degraded || !graph.coverage_gaps().is_empty() {
            writer.push(&coverage_card(&graph, delivery_degraded));
        }
        if !graph.diagnostics().is_empty() {
            writer.push(&diagnostics_card(&graph));
        }
        render_stage_list(&mut writer, &graph, verbose);
    }

    writer.finish()
}

const HTML_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<title>Astra Explain Analyze</title>
<style>
:root{color-scheme:dark;--bg:#0b1020;--surface:#121a2d;--surface-2:#19233b;--ink:#edf3ff;--muted:#9aa8c7;--line:#2a3858;--accent:#8c7bff;--accent-2:#48d6c7;--good:#4ade9a;--warn:#f4c76a;--bad:#ff7d91;--shadow:0 18px 50px rgba(0,0,0,.28);font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
@media (prefers-color-scheme:light){:root{color-scheme:light;--bg:#f4f7fc;--surface:#fff;--surface-2:#eef2fb;--ink:#17213a;--muted:#596781;--line:#d8e0f0;--accent:#5848d8;--accent-2:#087f78;--good:#087f5b;--warn:#9c6700;--bad:#bd304c;--shadow:0 18px 50px rgba(45,65,110,.12)}}
*{box-sizing:border-box}html{min-width:320px;background:var(--bg)}body{margin:0;background:radial-gradient(circle at 10% -10%,rgba(140,123,255,.22),transparent 38rem),radial-gradient(circle at 100% 0,rgba(72,214,199,.14),transparent 32rem),var(--bg);color:var(--ink);line-height:1.5}.report{width:min(1180px,100% - 32px);margin:0 auto;padding:32px 0 56px}.hero{position:relative;overflow:hidden;padding:36px 40px 32px;border:1px solid color-mix(in srgb,var(--accent) 35%,var(--line));border-radius:24px;background:linear-gradient(135deg,color-mix(in srgb,var(--surface) 86%,var(--accent) 14%),var(--surface));box-shadow:var(--shadow)}.hero:after{content:"";position:absolute;width:260px;height:260px;right:-80px;top:-120px;border-radius:50%;background:linear-gradient(135deg,rgba(140,123,255,.45),rgba(72,214,199,.12));filter:blur(2px);pointer-events:none}.eyebrow{position:relative;z-index:1;color:var(--accent-2);font-size:.72rem;font-weight:800;letter-spacing:.16em}.hero h1{position:relative;z-index:1;margin:8px 0 4px;font-size:clamp(1.8rem,4vw,3rem);letter-spacing:-.04em}.lede{position:relative;z-index:1;margin:0;color:var(--muted);max-width:62ch}.status-row{position:relative;z-index:1;display:flex;flex-wrap:wrap;gap:8px;margin-top:22px}.status{display:inline-flex;align-items:center;min-height:28px;padding:3px 10px;border:1px solid currentColor;border-radius:999px;font-size:.76rem;font-weight:750;letter-spacing:.02em}.status-recorded,.status-completed{color:var(--good);background:color-mix(in srgb,var(--good) 12%,transparent)}.status-partial,.status-warning,.status-incomplete{color:var(--warn);background:color-mix(in srgb,var(--warn) 12%,transparent)}.status-empty{color:var(--muted);background:color-mix(in srgb,var(--muted) 12%,transparent)}
.card{margin-top:18px;padding:22px 24px;border:1px solid var(--line);border-radius:18px;background:color-mix(in srgb,var(--surface) 94%,transparent);box-shadow:0 8px 26px rgba(0,0,0,.1)}.card h2{margin:0 0 14px;font-size:1.02rem;letter-spacing:-.01em}.metrics{display:grid;grid-template-columns:repeat(auto-fit,minmax(130px,1fr));gap:10px}.metric{min-width:0;padding:13px 14px;border:1px solid var(--line);border-radius:13px;background:var(--surface-2)}.metric-value{display:block;font-size:1.28rem;font-weight:800;letter-spacing:-.03em;overflow-wrap:anywhere}.metric-label{display:block;margin-top:3px;color:var(--muted);font-size:.73rem}.note{margin:14px 0 0;color:var(--muted);font-size:.84rem}.note strong{color:var(--ink)}.coverage{border-color:color-mix(in srgb,var(--warn) 46%,var(--line))}.coverage-list,.diagnostic-list{display:flex;flex-wrap:wrap;gap:8px;padding:0;margin:0;list-style:none}.coverage-list li,.diagnostic-list li{padding:7px 10px;border-radius:10px;background:color-mix(in srgb,var(--warn) 12%,var(--surface-2));color:var(--warn);font-size:.79rem}.diagnostic-list li{background:color-mix(in srgb,var(--bad) 11%,var(--surface-2));color:var(--bad)}.empty{text-align:center;padding:54px 24px}.empty-icon{display:grid;place-items:center;width:54px;height:54px;margin:0 auto 14px;border:1px solid var(--line);border-radius:16px;color:var(--accent);font-size:1.8rem}.empty p{max-width:66ch;margin:0 auto;color:var(--muted)}
.stages{margin-top:24px}.stages-heading{display:flex;align-items:baseline;justify-content:space-between;gap:12px;margin:0 2px 10px}.stages-heading h2{margin:0;font-size:1.05rem}.stages-heading span{color:var(--muted);font-size:.77rem}.stage{--indent:0px;position:relative;margin:8px 0 0;margin-left:var(--indent);border:1px solid var(--line);border-radius:14px;background:var(--surface);overflow:hidden;transition:border-color .18s ease,transform .18s ease,box-shadow .18s ease}.stage:hover{border-color:color-mix(in srgb,var(--accent) 55%,var(--line));box-shadow:0 8px 22px rgba(0,0,0,.13);transform:translateY(-1px)}.stage summary{display:grid;grid-template-columns:auto minmax(0,1fr) auto;align-items:center;gap:10px;padding:13px 15px;cursor:pointer;list-style:none}.stage summary::-webkit-details-marker{display:none}.stage summary:focus-visible{outline:2px solid var(--accent);outline-offset:-2px}.stage-marker{width:9px;height:9px;border-radius:50%;background:var(--muted);box-shadow:0 0 0 4px color-mix(in srgb,var(--muted) 14%,transparent)}.state-completed .stage-marker,.state-recorded .stage-marker,.state-succeeded .stage-marker{background:var(--good);box-shadow:0 0 0 4px color-mix(in srgb,var(--good) 14%,transparent)}.state-failed .stage-marker{background:var(--bad);box-shadow:0 0 0 4px color-mix(in srgb,var(--bad) 14%,transparent)}.state-waiting .stage-marker,.state-incomplete .stage-marker{background:var(--warn);box-shadow:0 0 0 4px color-mix(in srgb,var(--warn) 14%,transparent)}.stage-label{min-width:0;font-weight:720;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.stage-meta{color:var(--muted);font-size:.75rem;text-align:right;white-space:nowrap}.stage-body{padding:0 15px 15px 34px;border-top:1px solid var(--line);animation:reveal .2s ease-out}.stage-facts{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:8px;padding-top:13px}.fact{min-width:0}.fact-label{display:block;color:var(--muted);font-size:.7rem;text-transform:uppercase;letter-spacing:.08em}.fact-value{display:block;margin-top:2px;overflow-wrap:anywhere;font-size:.84rem}.timeline{height:7px;margin-top:14px;border-radius:999px;background:var(--surface-2);overflow:hidden}.timeline span{display:block;width:var(--bar);height:100%;border-radius:inherit;background:linear-gradient(90deg,var(--accent),var(--accent-2));transform-origin:left;animation:grow .55s ease-out}.detail-line{margin:12px 0 0;color:var(--muted);font-size:.78rem}.detail-line strong{color:var(--ink)}.chips{display:flex;flex-wrap:wrap;gap:6px;margin-top:7px}.chip{display:inline-block;max-width:100%;padding:4px 8px;border:1px solid var(--line);border-radius:8px;color:var(--muted);font-size:.75rem;overflow-wrap:anywhere}.warning-chip{border-color:color-mix(in srgb,var(--warn) 46%,var(--line));color:var(--warn)}.conflict{border-color:color-mix(in srgb,var(--bad) 52%,var(--line))}.orphan-heading{margin:22px 2px 8px;color:var(--warn);font-size:.86rem}.footer{margin-top:24px;color:var(--muted);font-size:.74rem;text-align:center}.footer a{color:var(--accent-2)}
@keyframes reveal{from{opacity:0;transform:translateY(-3px)}to{opacity:1;transform:none}}@keyframes grow{from{transform:scaleX(0)}to{transform:scaleX(1)}}@media (prefers-reduced-motion:reduce){*,*:before,*:after{animation-duration:.001ms!important;animation-iteration-count:1!important;scroll-behavior:auto!important;transition-duration:.001ms!important}}@media (max-width:640px){.report{width:min(100% - 18px,1180px);padding-top:10px}.hero{padding:26px 22px 24px;border-radius:18px}.card{padding:18px 16px;border-radius:15px}.stage{margin-left:0}.stage summary{grid-template-columns:auto minmax(0,1fr);padding:12px}.stage-meta{grid-column:2;text-align:left;white-space:normal}.stage-body{padding-left:30px}.metrics{grid-template-columns:repeat(2,minmax(0,1fr))}}
</style>
</head>
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

fn overview_card(graph: &ExplainAnalyzeGraphV1, delivery_degraded: bool) -> String {
    let timed = graph
        .nodes()
        .iter()
        .filter(|node| node.terminal_observed && node.duration_ms.is_some())
        .count();
    let clocks = graph
        .nodes()
        .iter()
        .map(|node| node.clock_domain_id.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let diagnostics = graph.diagnostics().len();
    let coverage = graph.coverage_gaps().len();
    let observation_gaps = diagnostics
        .saturating_add(coverage)
        .saturating_add(usize::from(delivery_degraded));
    let overlap = graph
        .max_concurrency()
        .map(|count| {
            if delivery_degraded || coverage > 0 {
                format!("≥{count} recorded")
            } else {
                count.to_string()
            }
        })
        .unwrap_or_else(|| "—".to_string());
    let duplicate = graph.duplicate_event_count();
    let mut note = String::new();
    if delivery_degraded {
        note.push_str("<p class=\"note\"><strong>Action needed:</strong> stream delivery stopped early; use the canonical JSON artifact when replaying the missing tail.</p>");
    } else if diagnostics > 0 {
        note.push_str("<p class=\"note\"><strong>Review the observation gaps below</strong> before treating timing as a complete account.</p>");
    } else if duplicate > 0 {
        note.push_str(&format!(
            "<p class=\"note\">{} duplicate runtime fact{} were collapsed safely.</p>",
            duplicate,
            if duplicate == 1 { "" } else { "s" }
        ));
    }
    format!(
        "<section class=\"card\"><h2>At a glance</h2><div class=\"metrics\"><div class=\"metric\"><span class=\"metric-value\">{}</span><span class=\"metric-label\">stages</span></div><div class=\"metric\"><span class=\"metric-value\">{}/{} </span><span class=\"metric-label\">timed spans</span></div><div class=\"metric\"><span class=\"metric-value\">{}</span><span class=\"metric-label\">clock domains</span></div><div class=\"metric\"><span class=\"metric-value\">{}</span><span class=\"metric-label\">max overlap</span></div><div class=\"metric\"><span class=\"metric-value\">{}</span><span class=\"metric-label\">observation gaps</span></div></div>{}</section>",
        graph.nodes().len(),
        timed,
        graph.nodes().len(),
        clocks,
        overlap,
        observation_gaps,
        note,
    )
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
        "<section class=\"card coverage\"><h2>Coverage to keep in mind</h2><ul class=\"coverage-list\">{items}</ul><p class=\"note\">These boundaries are intentionally marked instead of being guessed from wall-clock gaps.</p></section>"
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
        "<section class=\"card\"><h2>Observation gaps</h2><ul class=\"diagnostic-list\">{items}</ul><p class=\"note\">Malformed or conflicting facts stay visible and never rewrite an earlier accepted stage.</p></section>"
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
    let max_duration_by_clock =
        graph
            .nodes()
            .iter()
            .fold(HashMap::<&str, u64>::new(), |mut ends, node| {
                let duration = node.duration_ms.unwrap_or_else(|| {
                    node.end_elapsed_ms
                        .unwrap_or(node.start_elapsed_ms)
                        .saturating_sub(node.start_elapsed_ms)
                });
                ends.entry(node.clock_domain_id.as_str())
                    .and_modify(|current| *current = (*current).max(duration))
                    .or_insert(duration);
                ends
            });
    writer.push("<section class=\"stages\"><div class=\"stages-heading\"><h2>Stages</h2><span>select a row to inspect evidence</span></div>");

    while rendered < MAX_RENDER_NODES {
        let Some((index, depth)) = stack.pop() else {
            while orphan_cursor < graph.nodes().len() && visited.contains(&orphan_cursor) {
                orphan_cursor += 1;
            }
            let Some(next) = (orphan_cursor < graph.nodes().len()).then_some(orphan_cursor) else {
                break;
            };
            orphan_cursor += 1;
            if !orphan_heading {
                writer.push("<h3 class=\"orphan-heading\">Unlinked stages</h3>");
                orphan_heading = true;
            }
            stack.push((next, 0));
            continue;
        };
        if !visited.insert(index) {
            continue;
        }
        let Some(node) = graph.nodes().get(index) else {
            continue;
        };
        let max_duration = max_duration_by_clock
            .get(node.clock_domain_id.as_str())
            .copied()
            .unwrap_or(0);
        if !writer.push(&render_node(
            graph,
            node,
            index,
            depth,
            max_duration,
            verbose,
        )) {
            rendered += 1;
            break;
        }
        rendered += 1;

        let mut children = graph.children(index).to_vec();
        children.sort_by_key(|child_index| {
            graph
                .nodes()
                .get(*child_index)
                .map(|child| (child.start_elapsed_ms, *child_index))
                .unwrap_or((u64::MAX, *child_index))
        });
        let child_depth = depth.saturating_add(1).min(MAX_TREE_DEPTH);
        for child in children.into_iter().rev() {
            stack.push((child, child_depth));
        }
    }

    if rendered < graph.nodes().len() {
        writer.omit(graph.nodes().len().saturating_sub(rendered));
    }
    writer.push_reserved("</section>");
}

fn render_node(
    graph: &ExplainAnalyzeGraphV1,
    node: &ExplainAnalyzeProjectedNodeV1,
    index: usize,
    depth: usize,
    max_duration: u64,
    verbose: bool,
) -> String {
    let state = node_state(node.terminal_observed, node.outcome);
    let state_class = state_class(state);
    let duration = node
        .duration_ms
        .map(format_ms)
        .unwrap_or_else(|| "not measured".to_string());
    let end = node.end_elapsed_ms.unwrap_or(node.start_elapsed_ms);
    let bar = if max_duration == 0 {
        0
    } else {
        node.duration_ms
            .unwrap_or_else(|| end.saturating_sub(node.start_elapsed_ms))
            .saturating_mul(100)
            .checked_div(max_duration)
            .unwrap_or(0)
            .min(100)
    };
    let round = node
        .round_index
        .map(|round| format!("round {}", u64::from(round).saturating_add(1)));
    let attempt = node
        .attempt_index
        .map(|attempt| format!("attempt {}", u64::from(attempt).saturating_add(1)));
    let mut meta = vec![duration.clone(), state.to_string()];
    if let Some(round) = round {
        meta.push(round);
    }
    if let Some(attempt) = attempt {
        meta.push(attempt);
    }

    let mut body = format!(
        "<div class=\"stage-facts\"><div class=\"fact\"><span class=\"fact-label\">kind</span><span class=\"fact-value\">{}</span></div><div class=\"fact\"><span class=\"fact-label\">start</span><span class=\"fact-value\">{}</span></div><div class=\"fact\"><span class=\"fact-label\">clock</span><span class=\"fact-value\">{}</span></div><div class=\"fact\"><span class=\"fact-label\">stage index</span><span class=\"fact-value\">{index}</span></div></div><div class=\"timeline\" aria-label=\"relative duration\"><span style=\"--bar:{bar}%\"></span></div>",
        escape_html(kind_label(node.kind), MAX_TEXT_CHARS),
        format_ms(node.start_elapsed_ms),
        escape_html(&node.clock_domain_id, MAX_TEXT_CHARS),
    );
    body.push_str(&format!(
        "<p class=\"detail-line\"><strong>Outcome:</strong> {}</p>",
        escape_html(outcome_label(node.outcome), MAX_TEXT_CHARS)
    ));
    if node.conflicted {
        body.push_str("<p class=\"detail-line\"><span class=\"chip warning-chip\">conflicting facts were retained for review</span></p>");
    }
    if let Some(usage) = &node.usage {
        body.push_str(&format!(
            "<p class=\"detail-line\"><strong>Token usage:</strong> {}</p>",
            escape_html(&usage_summary(usage), MAX_TEXT_CHARS)
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

    let indent = depth.min(10) * 16;
    format!(
        "<details class=\"stage state-{state_class}{}\" style=\"--indent:{indent}px\" data-index=\"{index}\"><summary><span class=\"stage-marker\" aria-hidden=\"true\"></span><span class=\"stage-label\">{}</span><span class=\"stage-meta\">{}</span></summary><div class=\"stage-body\">{body}</div></details>",
        if node.parent_index.is_none() && node.parent_node_id.is_some() {
            " conflict"
        } else if node.conflicted {
            " conflict"
        } else {
            ""
        },
        escape_html(&node.label, MAX_TEXT_CHARS),
        escape_html(&meta.join(" · "), MAX_TEXT_CHARS),
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
        Some(ExplainAnalyzeOutcomeV1::Failed | ExplainAnalyzeOutcomeV1::Rejected) => "failed",
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
        EmergentSkills => "emergent skills",
        EmergentMemory => "emergent memory",
        EmergentSummary => "emergent summary",
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
        escaped.push_str("…");
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
                "<section class=\"card coverage\"><h2>Report bounded for safety</h2><p class=\"note\">{} stage detail{} omitted to keep this offline report readable and below 1 MiB. The canonical JSON artifact retains the complete fact stream.</p></section>",
                self.omitted,
                if self.omitted == 1 { " was" } else { "s were" }
            );
            if self.output.len().saturating_add(message.len() + 20) <= MAX_HTML_BYTES {
                self.output.push_str(&message);
            }
        }
        self.output.push_str("<footer class=\"footer\">Generated locally from canonical Explain Analyze facts · no network resources</footer></main></body></html>");
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        EXPLAIN_ANALYZE_SCHEMA_VERSION, ExplainAnalyzeCoverageGapV1, ExplainAnalyzeTransitionV1,
    };

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
        assert_eq!(html.matches("style=\"--bar:100%\"").count(), 2);
        assert_eq!(html.matches("style=\"--bar:50%\"").count(), 1);
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

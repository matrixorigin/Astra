//! Standalone HTML Explain Analyze rendering over canonical execution facts.
//!
//! The HTML report is deliberately a derived, local view.  It contains no
//! scripts, network requests, user supplied DOM identifiers, or unbounded
//! recursive markup.  A bounded writer keeps malformed or unexpectedly large
//! graphs from producing a partial document.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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
const MAX_TIMELINE_NODES: usize = 256;
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
        "<body><main class=\"report\"><header class=\"topbar\"><div class=\"brand\"><div class=\"eyebrow\">ASTRA / EXPLAIN ANALYZE</div><h1>Execution workspace</h1></div><div class=\"snapshot\"><span class=\"snapshot-mark\" aria-hidden=\"true\"></span><span>Saved snapshot</span><small>local report · canonical JSON retained</small></div></header>{}",
        summary_strip(&graph, &status, delivery_degraded),
    ));

    writer.push(&overview_card(&graph, delivery_degraded));

    let auxiliary_lines = crate::explain_analyze_report::auxiliary_usage_lines(&graph);
    if !auxiliary_lines.is_empty() {
        writer.push("<section class=\"panel\"><h2>Auxiliary model usage</h2>");
        for line in auxiliary_lines {
            writer.push(&format!("<p>{}</p>", escape_html(&line, usize::MAX)));
        }
        writer.push("</section>");
    }

    if graph.nodes().is_empty() {
        let message = if delivery_degraded {
            "Runtime facts were not recovered before stream delivery stopped. The canonical JSON artifact remains the source of truth for replay."
        } else {
            "No runtime facts were captured for this turn. Run Explain Analyze again after the session has started."
        };
        writer.push(&format!(
            "<section class=\"empty panel\"><div class=\"empty-icon\" aria-hidden=\"true\">∅</div><h2>No stages to display</h2><p>{}</p></section>",
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
<style media="not all">
:root{color-scheme:dark;--bg:#0b1020;--surface:#121a2d;--surface-2:#19233b;--ink:#edf3ff;--muted:#9aa8c7;--line:#2a3858;--accent:#8c7bff;--accent-2:#48d6c7;--good:#4ade9a;--warn:#f4c76a;--bad:#ff7d91;--shadow:0 18px 50px rgba(0,0,0,.28);font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
@media (prefers-color-scheme:light){:root{color-scheme:light;--bg:#f4f7fc;--surface:#fff;--surface-2:#eef2fb;--ink:#17213a;--muted:#596781;--line:#d8e0f0;--accent:#5848d8;--accent-2:#087f78;--good:#087f5b;--warn:#9c6700;--bad:#bd304c;--shadow:0 18px 50px rgba(45,65,110,.12)}}
*{box-sizing:border-box}html{min-width:320px;background:var(--bg)}body{margin:0;background:radial-gradient(circle at 10% -10%,rgba(140,123,255,.22),transparent 38rem),radial-gradient(circle at 100% 0,rgba(72,214,199,.14),transparent 32rem),var(--bg);color:var(--ink);line-height:1.5}.report{width:min(1180px,100% - 32px);margin:0 auto;padding:32px 0 56px}.hero{position:relative;overflow:hidden;padding:36px 40px 32px;border:1px solid color-mix(in srgb,var(--accent) 35%,var(--line));border-radius:24px;background:linear-gradient(135deg,color-mix(in srgb,var(--surface) 86%,var(--accent) 14%),var(--surface));box-shadow:var(--shadow)}.hero:after{content:"";position:absolute;width:260px;height:260px;right:-80px;top:-120px;border-radius:50%;background:linear-gradient(135deg,rgba(140,123,255,.45),rgba(72,214,199,.12));filter:blur(2px);pointer-events:none}.eyebrow{position:relative;z-index:1;color:var(--accent-2);font-size:.72rem;font-weight:800;letter-spacing:.16em}.hero h1{position:relative;z-index:1;margin:8px 0 4px;font-size:clamp(1.8rem,4vw,3rem);letter-spacing:-.04em}.lede{position:relative;z-index:1;margin:0;color:var(--muted);max-width:62ch}.status-row{position:relative;z-index:1;display:flex;flex-wrap:wrap;gap:8px;margin-top:22px}.status{display:inline-flex;align-items:center;min-height:28px;padding:3px 10px;border:1px solid currentColor;border-radius:999px;font-size:.76rem;font-weight:750;letter-spacing:.02em}.status-recorded,.status-completed{color:var(--good);background:color-mix(in srgb,var(--good) 12%,transparent)}.status-partial,.status-warning,.status-incomplete{color:var(--warn);background:color-mix(in srgb,var(--warn) 12%,transparent)}.status-empty{color:var(--muted);background:color-mix(in srgb,var(--muted) 12%,transparent)}
.card{margin-top:18px;padding:22px 24px;border:1px solid var(--line);border-radius:18px;background:color-mix(in srgb,var(--surface) 94%,transparent);box-shadow:0 8px 26px rgba(0,0,0,.1)}.card h2{margin:0 0 14px;font-size:1.02rem;letter-spacing:-.01em}.metrics{display:grid;grid-template-columns:repeat(auto-fit,minmax(130px,1fr));gap:10px}.metric{min-width:0;padding:13px 14px;border:1px solid var(--line);border-radius:13px;background:var(--surface-2)}.metric-value{display:block;font-size:1.28rem;font-weight:800;letter-spacing:-.03em;overflow-wrap:anywhere}.metric-label{display:block;margin-top:3px;color:var(--muted);font-size:.73rem}.note{margin:14px 0 0;color:var(--muted);font-size:.84rem}.note strong{color:var(--ink)}.coverage{border-color:color-mix(in srgb,var(--warn) 46%,var(--line))}.coverage-list,.diagnostic-list{display:flex;flex-wrap:wrap;gap:8px;padding:0;margin:0;list-style:none}.coverage-list li,.diagnostic-list li{padding:7px 10px;border-radius:10px;background:color-mix(in srgb,var(--warn) 12%,var(--surface-2));color:var(--warn);font-size:.79rem}.diagnostic-list li{background:color-mix(in srgb,var(--bad) 11%,var(--surface-2));color:var(--bad)}.empty{text-align:center;padding:54px 24px}.empty-icon{display:grid;place-items:center;width:54px;height:54px;margin:0 auto 14px;border:1px solid var(--line);border-radius:16px;color:var(--accent);font-size:1.8rem}.empty p{max-width:66ch;margin:0 auto;color:var(--muted)}
.stages{margin-top:24px}.stages-heading{display:flex;align-items:baseline;justify-content:space-between;gap:12px;margin:0 2px 10px}.stages-heading h2{margin:0;font-size:1.05rem}.stages-heading span{color:var(--muted);font-size:.77rem}.stage{--indent:0px;position:relative;margin:8px 0 0;margin-left:var(--indent);border:1px solid var(--line);border-radius:14px;background:var(--surface);overflow:hidden;transition:border-color .18s ease,transform .18s ease,box-shadow .18s ease}.stage:hover{border-color:color-mix(in srgb,var(--accent) 55%,var(--line));box-shadow:0 8px 22px rgba(0,0,0,.13);transform:translateY(-1px)}.stage summary{display:grid;grid-template-columns:auto minmax(0,1fr) auto;align-items:center;gap:10px;padding:13px 15px;cursor:pointer;list-style:none}.stage summary::-webkit-details-marker{display:none}.stage summary:focus-visible{outline:2px solid var(--accent);outline-offset:-2px}.stage-marker{width:9px;height:9px;border-radius:50%;background:var(--muted);box-shadow:0 0 0 4px color-mix(in srgb,var(--muted) 14%,transparent)}.state-completed .stage-marker,.state-recorded .stage-marker,.state-succeeded .stage-marker{background:var(--good);box-shadow:0 0 0 4px color-mix(in srgb,var(--good) 14%,transparent)}.state-failed .stage-marker{background:var(--bad);box-shadow:0 0 0 4px color-mix(in srgb,var(--bad) 14%,transparent)}.state-waiting .stage-marker,.state-incomplete .stage-marker{background:var(--warn);box-shadow:0 0 0 4px color-mix(in srgb,var(--warn) 14%,transparent)}.stage-label{min-width:0;font-weight:720;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.stage-meta{color:var(--muted);font-size:.75rem;text-align:right;white-space:nowrap}.stage-body{padding:0 15px 15px 34px;border-top:1px solid var(--line);animation:reveal .2s ease-out}.stage-facts{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:8px;padding-top:13px}.fact{min-width:0}.fact-label{display:block;color:var(--muted);font-size:.7rem;text-transform:uppercase;letter-spacing:.08em}.fact-value{display:block;margin-top:2px;overflow-wrap:anywhere;font-size:.84rem}.timeline{height:7px;margin-top:14px;border-radius:999px;background:var(--surface-2);overflow:hidden}.timeline span{display:block;width:var(--bar);height:100%;border-radius:inherit;background:linear-gradient(90deg,var(--accent),var(--accent-2));transform-origin:left;animation:grow .55s ease-out}.detail-line{margin:12px 0 0;color:var(--muted);font-size:.78rem}.detail-line strong{color:var(--ink)}.chips{display:flex;flex-wrap:wrap;gap:6px;margin-top:7px}.chip{display:inline-block;max-width:100%;padding:4px 8px;border:1px solid var(--line);border-radius:8px;color:var(--muted);font-size:.75rem;overflow-wrap:anywhere}.warning-chip{border-color:color-mix(in srgb,var(--warn) 46%,var(--line));color:var(--warn)}.conflict{border-color:color-mix(in srgb,var(--bad) 52%,var(--line))}.orphan-heading{margin:22px 2px 8px;color:var(--warn);font-size:.86rem}.footer{margin-top:24px;color:var(--muted);font-size:.74rem;text-align:center}.footer a{color:var(--accent-2)}
@keyframes reveal{from{opacity:0;transform:translateY(-3px)}to{opacity:1;transform:none}}@keyframes grow{from{transform:scaleX(0)}to{transform:scaleX(1)}}@media (prefers-reduced-motion:reduce){*,*:before,*:after{animation-duration:.001ms!important;animation-iteration-count:1!important;scroll-behavior:auto!important;transition-duration:.001ms!important}}@media (max-width:640px){.report{width:min(100% - 18px,1180px);padding-top:10px}.hero{padding:26px 22px 24px;border-radius:18px}.card{padding:18px 16px;border-radius:15px}.stage{margin-left:0}.stage summary{grid-template-columns:auto minmax(0,1fr);padding:12px}.stage-meta{grid-column:2;text-align:left;white-space:normal}.stage-body{padding-left:30px}.metrics{grid-template-columns:repeat(2,minmax(0,1fr))}}
</style>
<style>
/* The report is an inspection surface: dense evidence first, decoration second. */
*{box-sizing:border-box}html{min-width:320px}body{margin:0;color:var(--ink)}.report{margin:0 auto}
:root{color-scheme:dark}
:root{--bg:#0b1017;--surface:#111923;--surface-2:#17212d;--surface-3:#1d2a38;--ink:#edf4fb;--muted:#8d9cad;--line:#273442;--line-strong:#3a4b5d;--accent:#83b8ff;--accent-2:#72e0c2;--good:#58d49a;--warn:#f3c66d;--bad:#ff788b;--shadow:0 20px 48px rgba(0,0,0,.22);font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
@media (prefers-color-scheme:light){:root{color-scheme:light;--bg:#f5f7fa;--surface:#fff;--surface-2:#f0f4f7;--surface-3:#e7eef4;--ink:#15202b;--muted:#617181;--line:#d8e1e8;--line-strong:#b8c8d5;--accent:#246bc2;--accent-2:#087f70;--good:#087f5b;--warn:#996a00;--bad:#bd304c;--shadow:0 18px 42px rgba(34,58,80,.1)}}
html,body{background:var(--bg)}body{background:linear-gradient(180deg,color-mix(in srgb,var(--surface-2) 32%,var(--bg)),var(--bg) 24rem);line-height:1.45}.report{width:min(1360px,calc(100% - 40px));padding:22px 0 54px}.topbar{display:flex;align-items:flex-start;justify-content:space-between;gap:24px;padding:4px 0 18px;border-bottom:1px solid var(--line)}.brand{min-width:0}.eyebrow{color:var(--accent-2);font-size:.68rem;font-weight:800;letter-spacing:.16em}.topbar h1{margin:6px 0 0;font-size:clamp(1.45rem,3vw,2.1rem);letter-spacing:-.035em}.snapshot{display:grid;grid-template-columns:auto 1fr;column-gap:8px;align-items:center;min-width:190px;color:var(--ink);font-size:.78rem}.snapshot small{grid-column:2;color:var(--muted);font-size:.7rem}.snapshot-mark{grid-row:1 / span 2;width:9px;height:9px;border-radius:50%;background:var(--accent-2);box-shadow:0 0 0 5px color-mix(in srgb,var(--accent-2) 16%,transparent)}.summary-strip{display:grid;grid-template-columns:minmax(240px,1.6fr) repeat(5,minmax(90px,1fr));gap:1px;margin-top:18px;border:1px solid var(--line);border-radius:14px;overflow:hidden;background:var(--line);box-shadow:var(--shadow)}.summary-strip>div{min-width:0;padding:15px 16px;background:var(--surface)}.summary-result{display:flex;flex-direction:column;justify-content:center}.result-line{display:flex;align-items:center;flex-wrap:wrap;gap:8px}.result{font-size:1.1rem;font-weight:800;letter-spacing:-.02em;text-transform:capitalize}.result-completed{color:var(--good)}.result-failed{color:var(--bad)}.result-waiting,.result-incomplete,.result-cancelled{color:var(--warn)}.capture{padding:3px 8px;border:1px solid currentColor;border-radius:999px;font-size:.68rem;font-weight:750;letter-spacing:.02em;text-transform:capitalize}.capture-recorded{color:var(--good);background:color-mix(in srgb,var(--good) 10%,transparent)}.capture-partial,.capture-incomplete{color:var(--warn);background:color-mix(in srgb,var(--warn) 10%,transparent)}.summary-context{display:block;margin-top:6px;color:var(--muted);font-size:.7rem}.summary-stat{display:flex;flex-direction:column;justify-content:center}.summary-stat strong{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:1.02rem;font-variant-numeric:tabular-nums;letter-spacing:-.03em;overflow-wrap:anywhere}.summary-stat span{margin-top:4px;color:var(--muted);font-size:.68rem}.panel{margin-top:16px;padding:18px 20px;border:1px solid var(--line);border-radius:14px;background:var(--surface);box-shadow:0 8px 26px rgba(0,0,0,.09)}.panel-heading{display:flex;align-items:flex-start;justify-content:space-between;gap:12px}.section-kicker{display:block;color:var(--accent-2);font-size:.65rem;font-weight:800;letter-spacing:.12em;text-transform:uppercase}.panel h2{margin:3px 0 0;font-size:1rem;letter-spacing:-.015em}.health-mark,.alert-mark{display:grid;place-items:center;width:24px;height:24px;border:1px solid color-mix(in srgb,var(--good) 45%,var(--line));border-radius:50%;color:var(--good);font-size:.78rem;font-weight:800}.alert-mark{border-color:color-mix(in srgb,var(--warn) 50%,var(--line));color:var(--warn)}.alert-mark.bad{border-color:color-mix(in srgb,var(--bad) 50%,var(--line));color:var(--bad)}.health-grid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:1px;margin-top:16px;border:1px solid var(--line);background:var(--line)}.health-grid>div{min-width:0;padding:11px 12px;background:var(--surface-2)}.health-grid strong{display:block;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.92rem;font-variant-numeric:tabular-nums;overflow-wrap:anywhere}.health-grid span{display:block;margin-top:3px;color:var(--muted);font-size:.69rem}.note{margin:13px 0 0;color:var(--muted);font-size:.78rem}.note strong{color:var(--ink)}.coverage,.diagnostics{background:color-mix(in srgb,var(--surface) 92%,var(--warn) 8%)}.diagnostics{background:color-mix(in srgb,var(--surface) 92%,var(--bad) 8%)}.coverage-list,.diagnostic-list{display:flex;flex-wrap:wrap;gap:7px;padding:0;margin:14px 0 0;list-style:none}.coverage-list li,.diagnostic-list li{padding:6px 9px;border:1px solid color-mix(in srgb,var(--warn) 35%,var(--line));border-radius:8px;background:color-mix(in srgb,var(--warn) 9%,var(--surface-2));color:var(--warn);font-size:.74rem}.diagnostic-list li{border-color:color-mix(in srgb,var(--bad) 35%,var(--line));background:color-mix(in srgb,var(--bad) 8%,var(--surface-2));color:var(--bad)}.empty{text-align:center;padding:48px 22px}.empty-icon{display:grid;place-items:center;width:44px;height:44px;margin:0 auto 12px;border:1px solid var(--line);border-radius:50%;color:var(--accent);font-size:1.45rem}.empty h2{margin:0}.empty p{max-width:66ch;margin:8px auto 0;color:var(--muted);font-size:.82rem}.stages{padding:18px 0 0;border:0;background:transparent;box-shadow:none}.stages-heading{display:flex;align-items:flex-end;justify-content:space-between;gap:16px;margin:0 2px 10px}.stages-heading h2{margin:3px 0 0;font-size:1.05rem}.stages-heading>span{color:var(--muted);font-size:.72rem;text-align:right}.stage-table-head{display:grid;grid-template-columns:32px minmax(0,1fr) 100px 120px 128px;gap:12px;padding:0 14px 7px;color:var(--muted);font-size:.65rem;font-weight:750;letter-spacing:.08em;text-transform:uppercase}.stage-row{--indent:0px;position:relative;margin:0 0 0 var(--indent);border:0;border-bottom:1px solid var(--line);border-radius:0;background:transparent;overflow:visible;box-shadow:none;transition:background-color .16s ease}.stage-row:hover{border-color:var(--line);box-shadow:none;transform:none;background:color-mix(in srgb,var(--accent) 4%,transparent)}.stage-row summary{display:grid;grid-template-columns:32px minmax(0,1fr) 100px 120px 128px;gap:12px;align-items:center;min-height:42px;padding:7px 14px;cursor:pointer;list-style:none}.stage-row summary::-webkit-details-marker{display:none}.stage-row summary:focus-visible{outline:2px solid var(--accent);outline-offset:-2px}.stage-leading{display:flex;align-items:center;gap:8px}.stage-toggle{display:inline-block;width:10px;color:var(--muted);font-size:1.05rem;line-height:1;transition:transform .16s ease}.stage-row[open] .stage-toggle{transform:rotate(90deg);color:var(--accent)}.stage-marker{width:7px;height:7px;border-radius:50%;background:var(--muted)}.state-completed .stage-marker{background:var(--good)}.state-failed .stage-marker{background:var(--bad)}.state-waiting .stage-marker,.state-incomplete .stage-marker{background:var(--warn)}.stage-label{min-width:0;display:flex;align-items:baseline;gap:8px}.stage-name{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:.84rem;font-weight:700}.stage-kind{flex:none;color:var(--muted);font-size:.67rem;white-space:nowrap}.stage-duration,.stage-tokens,.stage-outcome{min-width:0;color:var(--muted);font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.72rem;font-variant-numeric:tabular-nums;overflow-wrap:anywhere}.stage-outcome{display:flex;align-items:center;gap:6px;color:var(--ink);text-transform:capitalize}.outcome-dot{width:6px;height:6px;flex:none;border-radius:50%;background:var(--muted)}.state-completed .outcome-dot{background:var(--good)}.state-failed .outcome-dot{background:var(--bad)}.state-waiting .outcome-dot,.state-incomplete .outcome-dot{background:var(--warn)}.stage-body{margin:0 14px 10px 44px;padding:14px 0 3px;border-top:1px solid var(--line);animation:none}.stage-body-head{margin-bottom:13px}.full-label{margin-top:4px;color:var(--ink);font-size:.84rem;line-height:1.5;overflow-wrap:anywhere}.stage-facts{display:grid;grid-template-columns:repeat(auto-fit,minmax(125px,1fr));gap:8px;padding-top:0}.fact{min-width:0}.fact-label{display:block;color:var(--muted);font-size:.64rem;letter-spacing:.08em;text-transform:uppercase}.fact-value{display:block;margin-top:3px;color:var(--ink);font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.75rem;font-variant-numeric:tabular-nums;overflow-wrap:anywhere}.timeline-inline{height:5px;margin-top:14px;border-radius:999px;background:var(--surface-3);overflow:hidden}.timeline-inline span{display:block;width:var(--bar);height:100%;border-radius:inherit;background:linear-gradient(90deg,var(--accent),var(--accent-2))}.timeline-inline-unmeasured{height:auto;padding:7px 9px;color:var(--muted);font-size:.7rem}.detail-line{margin:11px 0 0;color:var(--muted);font-size:.75rem;overflow-wrap:anywhere}.detail-line strong{color:var(--ink)}.chips{display:flex;flex-wrap:wrap;gap:6px;margin-top:7px}.chip{display:inline-block;max-width:100%;padding:4px 8px;border:1px solid var(--line);border-radius:7px;color:var(--muted);font-size:.71rem;overflow-wrap:anywhere}.warning-chip{border-color:color-mix(in srgb,var(--warn) 46%,var(--line));color:var(--warn)}.conflict{border-color:color-mix(in srgb,var(--bad) 52%,var(--line))}.orphan-heading{margin:20px 2px 7px;color:var(--warn);font-size:.78rem;letter-spacing:.04em;text-transform:uppercase}.timeline-panel{padding:0;overflow:hidden;background:var(--surface)}.timeline-panel>summary{display:flex;align-items:center;gap:9px;padding:13px 14px;cursor:pointer;list-style:none}.timeline-panel>summary::-webkit-details-marker{display:none}.timeline-panel>summary:focus-visible{outline:2px solid var(--accent);outline-offset:-2px}.timeline-title{font-size:.79rem;font-weight:750}.timeline-summary{margin-left:auto;color:var(--muted);font-size:.68rem}.timeline-body{padding:0 14px 14px;border-top:1px solid var(--line)}.clock-group{padding-top:13px}.clock-heading{display:flex;justify-content:space-between;gap:12px;margin-bottom:7px;color:var(--muted);font-size:.68rem}.clock-heading strong{color:var(--ink);font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.7rem}.timeline-row{display:grid;grid-template-columns:minmax(130px,1fr) minmax(150px,3fr) 58px;gap:10px;align-items:center;min-height:28px;border-top:1px solid color-mix(in srgb,var(--line) 65%,transparent)}.timeline-name{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--muted);font-size:.71rem}.timeline-track{position:relative;display:block;height:8px;border-radius:999px;background:var(--surface-3);overflow:hidden}.timeline-bar{position:absolute;top:0;height:100%;min-width:2px;border-radius:inherit;background:var(--accent)}.timeline-bar.state-completed{background:var(--good)}.timeline-bar.state-failed{background:var(--bad)}.timeline-bar.state-waiting,.timeline-bar.state-incomplete{background:var(--warn)}.timeline-duration{color:var(--muted);font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.68rem;text-align:right}.timeline-unmeasured{display:flex;align-items:center;height:auto;min-height:24px;padding:3px 7px;color:var(--muted);font-size:.66rem}.timeline-omitted{margin:12px 0 0;color:var(--muted);font-size:.7rem}.footer{margin-top:28px;padding-top:14px;border-top:1px solid var(--line);color:var(--muted);font-size:.68rem;text-align:left}.footer a{color:var(--accent-2)}
@media (prefers-reduced-motion:reduce){.stage-toggle{transition:none!important}}
@media (max-width:900px){.summary-strip{grid-template-columns:minmax(220px,1.4fr) repeat(3,minmax(90px,1fr))}.summary-strip .summary-stat:nth-last-child(-n+2){display:none}.stage-table-head,.stage-row summary{grid-template-columns:30px minmax(0,1fr) 86px 104px 112px}.stage-kind{display:none}}
@media (max-width:640px){.report{width:calc(100% - 18px);padding-top:12px}.topbar{gap:14px}.snapshot{min-width:auto}.snapshot small{display:none}.summary-strip{grid-template-columns:repeat(2,minmax(0,1fr));margin-top:14px}.summary-result{grid-column:1 / -1}.summary-strip .summary-stat:nth-last-child(-n+2){display:flex}.health-grid{grid-template-columns:repeat(2,minmax(0,1fr))}.panel{padding:15px 14px}.stages{padding-left:0;padding-right:0}.stages-heading{align-items:flex-start;flex-direction:column;gap:4px}.stages-heading>span{text-align:left}.stage-table-head{display:none}.stage-row summary{grid-template-columns:26px minmax(0,1fr) auto;grid-template-areas:"lead label duration" "lead outcome outcome";column-gap:9px;row-gap:3px;padding:8px 6px}.stage-leading{grid-area:lead;align-self:start;padding-top:4px}.stage-label{grid-area:label;display:block}.stage-name{display:block;white-space:normal;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical}.stage-duration{grid-area:duration;align-self:start;text-align:right}.stage-tokens{display:none}.stage-outcome{grid-area:outcome;justify-self:start;font-size:.67rem}.stage-body{margin-left:35px;margin-right:6px}.stage-facts{grid-template-columns:repeat(2,minmax(0,1fr))}.timeline-panel{margin-left:0;margin-right:0}.timeline-row{grid-template-columns:minmax(92px,1.1fr) minmax(82px,1.7fr) 48px;gap:7px}.timeline-summary{display:none}.timeline-name{font-size:.66rem}}
</style>
<style>
.timeline-bar{min-width:0}.timeline-instant{position:absolute;top:-2px;display:block;width:2px;height:12px;transform:translateX(-1px);border-radius:2px;background:var(--accent);box-shadow:0 0 0 2px color-mix(in srgb,var(--accent) 18%,transparent)}.timeline-instant.state-completed{background:var(--good);box-shadow:0 0 0 2px color-mix(in srgb,var(--good) 18%,transparent)}.timeline-instant.state-failed{background:var(--bad);box-shadow:0 0 0 2px color-mix(in srgb,var(--bad) 18%,transparent)}.timeline-instant.state-waiting,.timeline-instant.state-incomplete{background:var(--warn);box-shadow:0 0 0 2px color-mix(in srgb,var(--warn) 18%,transparent)}
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

fn summary_strip(
    graph: &ExplainAnalyzeGraphV1,
    status: &ReportStatus,
    delivery_degraded: bool,
) -> String {
    let primary = graph
        .nodes()
        .iter()
        .find(|node| node.kind == ExplainAnalyzeNodeKindV1::Turn);
    let (result_class, result_label) = primary
        .map(|node| {
            (
                state_class(node_state(node.terminal_observed, node.outcome)),
                outcome_label(node.outcome),
            )
        })
        .unwrap_or(("incomplete", "outcome unavailable"));
    let wall_time = primary
        .and_then(|node| node.duration_ms)
        .map(format_ms)
        .unwrap_or_else(|| "—".to_string());
    let attempts = graph
        .nodes()
        .iter()
        .filter(|node| node.kind == ExplainAnalyzeNodeKindV1::ProviderAttempt)
        .count();
    let reported_attempts = graph
        .nodes()
        .iter()
        .filter(|node| {
            node.kind == ExplainAnalyzeNodeKindV1::ProviderAttempt && node.usage.is_some()
        })
        .count();
    let usage = if attempts == 0 {
        "—".to_string()
    } else {
        format!("{reported_attempts}/{attempts}")
    };
    let timed = graph
        .nodes()
        .iter()
        .filter(|node| node.terminal_observed && node.duration_ms.is_some())
        .count();
    let gaps = graph
        .diagnostics()
        .len()
        .saturating_add(graph.coverage_gaps().len())
        .saturating_add(usize::from(delivery_degraded));
    let capture_class = if delivery_degraded || status.class == "empty" {
        "incomplete"
    } else {
        status.class
    };
    let capture_label = if delivery_degraded {
        "delivery interrupted"
    } else {
        status.label
    };
    format!(
        "<section class=\"summary-strip\" aria-label=\"Execution summary\"><div class=\"summary-result\"><div class=\"result-line\"><span class=\"result result-{result_class}\">{}</span><span class=\"capture capture-{capture_class}\">{}</span></div><span class=\"summary-context\">Task outcome and capture completeness are shown separately.</span></div><div class=\"summary-stat\"><strong>{wall_time}</strong><span>turn wall time</span></div><div class=\"summary-stat\"><strong>{}</strong><span>provider attempts</span></div><div class=\"summary-stat\"><strong>{usage}</strong><span>usage coverage</span></div><div class=\"summary-stat\"><strong>{timed}/{} </strong><span>timed stages</span></div><div class=\"summary-stat\"><strong>{gaps}</strong><span>observation gaps</span></div></section>",
        escape_html(result_label, MAX_TEXT_CHARS),
        escape_html(capture_label, MAX_TEXT_CHARS),
        attempts,
        graph.nodes().len(),
    )
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
        "<section class=\"panel health-panel\"><div class=\"panel-heading\"><div><span class=\"section-kicker\">Capture health</span><h2>What this report can prove</h2></div><span class=\"health-mark\" aria-hidden=\"true\">{}</span></div><div class=\"health-grid\"><div><strong>{}/{} </strong><span>timed stages</span></div><div><strong>{}</strong><span>clock domains</span></div><div><strong>{}</strong><span>recorded overlap</span></div><div><strong>{}</strong><span>observation gaps</span></div></div>{}</section>",
        if !delivery_degraded && graph.diagnostics().is_empty() && graph.coverage_gaps().is_empty()
        {
            "·"
        } else {
            "!"
        },
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
        "<section class=\"panel alert-panel coverage\"><div class=\"panel-heading\"><div><span class=\"section-kicker\">Action needed</span><h2>Coverage boundaries</h2></div><span class=\"alert-mark\" aria-hidden=\"true\">!</span></div><ul class=\"coverage-list\">{items}</ul><p class=\"note\">These boundaries are marked explicitly instead of being guessed from wall-clock gaps.</p></section>"
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
        "<section class=\"panel alert-panel diagnostics\"><div class=\"panel-heading\"><div><span class=\"section-kicker\">Review before trusting timing</span><h2>Observation gaps</h2></div><span class=\"alert-mark bad\" aria-hidden=\"true\">!</span></div><ul class=\"diagnostic-list\">{items}</ul><p class=\"note\">Malformed or conflicting facts stay visible and never rewrite an earlier accepted stage.</p></section>"
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
    let clock_labels = clock_labels(graph);
    let timeline_markup = render_timeline_panel(graph, &clock_labels);
    let remaining = writer.remaining_capacity();
    let include_timeline =
        timeline_markup.is_empty() || timeline_markup.len() <= remaining.saturating_sub(4_096);
    let timeline_len = if include_timeline {
        timeline_markup.len()
    } else {
        0
    };
    let rows_budget = remaining.saturating_sub(timeline_len).saturating_sub(2_048);
    let mut rows = String::with_capacity(rows_budget.min(16 * 1024));
    let mut capacity_reached = false;

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
        let max_duration = max_duration_by_clock
            .get(node.clock_domain_id.as_str())
            .copied()
            .unwrap_or(0);
        let row = render_node(
            graph,
            node,
            index,
            depth,
            max_duration,
            &clock_labels,
            verbose,
        );
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
            capacity_reached = true;
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

    let omitted = graph.nodes().len().saturating_sub(rendered);
    if capacity_reached && omitted == 0 {
        // This can only happen when the hard node limit is reached at the
        // same time as the graph size; keep the state explicit for reviewers.
        debug_assert!(rendered == MAX_RENDER_NODES);
    }
    let heading = format!(
        "<section class=\"stages panel\"><div class=\"stages-heading\"><div><span class=\"section-kicker\">Primary view</span><h2>Execution tree</h2></div><span>Showing {rendered} of {} stages · select a row for evidence</span></div><div class=\"stage-table-head\"><span></span><span>Stage</span><span>Duration</span><span>Tokens</span><span>Outcome</span></div>{}",
        graph.nodes().len(),
        if !include_timeline && !timeline_markup.is_empty() {
            "<p class=\"timeline-omitted\">Secondary timeline omitted to preserve the report bound; the execution tree remains available.</p>"
        } else {
            ""
        },
    );
    if writer.push(&heading) {
        if include_timeline {
            writer.push(&timeline_markup);
        }
        let rows_written = writer.push(&rows);
        if rows_written {
            if omitted > 0 {
                writer.omit(omitted);
            }
        } else {
            writer.omit(graph.nodes().len());
        }
        writer.push_reserved("</section>");
    } else {
        writer.omit(graph.nodes().len());
    }
}

fn clock_labels(graph: &ExplainAnalyzeGraphV1) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    for node in graph.nodes() {
        let ordinal = labels.len();
        labels
            .entry(node.clock_domain_id.clone())
            .or_insert_with(|| format!("clock {}", alpha_label(ordinal)));
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

fn timeline_interval(node: &ExplainAnalyzeProjectedNodeV1) -> Option<(u64, u64)> {
    let end = node.end_elapsed_ms.or_else(|| {
        node.duration_ms
            .map(|duration| node.start_elapsed_ms.saturating_add(duration))
    })?;
    (end >= node.start_elapsed_ms).then_some((node.start_elapsed_ms, end))
}

fn render_timeline_panel(
    graph: &ExplainAnalyzeGraphV1,
    clock_labels: &HashMap<String, String>,
) -> String {
    let mut groups = BTreeMap::<&str, Vec<(usize, &ExplainAnalyzeProjectedNodeV1)>>::new();
    let mut selected = 0usize;
    for (index, node) in graph.nodes().iter().enumerate() {
        if selected >= MAX_TIMELINE_NODES {
            break;
        }
        groups
            .entry(node.clock_domain_id.as_str())
            .or_default()
            .push((index, node));
        selected += 1;
    }
    if groups.is_empty() {
        return String::new();
    }

    let mut markup = String::from(
        "<details class=\"timeline-panel panel\"><summary><span class=\"stage-toggle\" aria-hidden=\"true\">›</span><span class=\"timeline-title\">Timeline by clock domain</span><span class=\"timeline-summary\">measured intervals only</span></summary><div class=\"timeline-body\">",
    );
    for (clock, mut entries) in groups {
        entries.sort_by_key(|(index, node)| (node.start_elapsed_ms, *index));
        let min_start = entries
            .iter()
            .map(|(_, node)| node.start_elapsed_ms)
            .min()
            .unwrap_or(0);
        let max_end = entries
            .iter()
            .filter_map(|(_, node)| timeline_interval(node).map(|(_, end)| end))
            .max()
            .unwrap_or(min_start.saturating_add(1));
        let span = max_end.saturating_sub(min_start).max(1);
        let clock_label = clock_labels
            .get(clock)
            .map(String::as_str)
            .unwrap_or("clock ?");
        markup.push_str(&format!(
            "<section class=\"clock-group\"><div class=\"clock-heading\"><strong>{}</strong><span>{} stage{}</span></div>",
            escape_html(clock_label, MAX_TEXT_CHARS),
            entries.len(),
            if entries.len() == 1 { "" } else { "s" },
        ));
        for (index, node) in entries {
            let state = node_state(node.terminal_observed, node.outcome);
            let state_class = state_class(state);
            let label = escape_html(&node.label, 180);
            markup.push_str(&format!(
                "<div class=\"timeline-row\" data-index=\"{index}\"><span class=\"timeline-name\" title=\"{label}\">{label}</span>",
            ));
            if let Some((start, end)) = timeline_interval(node) {
                let left_basis = start
                    .saturating_sub(min_start)
                    .saturating_mul(10_000)
                    .checked_div(span)
                    .unwrap_or(0)
                    .min(10_000);
                let width_basis = end
                    .saturating_sub(start)
                    .saturating_mul(10_000)
                    .checked_div(span)
                    .unwrap_or(0)
                    .min(10_000 - left_basis);
                let duration_label = format_ms(end.saturating_sub(start));
                let position = left_basis as f64 / 100.0;
                let width = width_basis as f64 / 100.0;
                if start == end {
                    markup.push_str(&format!(
                        "<span class=\"timeline-track\"><span class=\"timeline-instant state-{state_class}\" style=\"left:{position:.2}%\" aria-label=\"instant event\"></span></span><span class=\"timeline-duration\">{duration_label} · instant</span></div>",
                    ));
                } else if width_basis == 0 {
                    markup.push_str(&format!(
                        "<span class=\"timeline-track\"><span class=\"timeline-instant timeline-micro state-{state_class}\" style=\"left:{position:.2}%\" aria-label=\"very short interval\"></span></span><span class=\"timeline-duration\">{duration_label} · short</span></div>",
                    ));
                } else {
                    markup.push_str(&format!(
                        "<span class=\"timeline-track\"><span class=\"timeline-bar state-{state_class}\" style=\"left:{position:.2}%;width:{width:.2}%\" aria-hidden=\"true\"></span></span><span class=\"timeline-duration\">{duration_label}</span></div>",
                    ));
                }
            } else {
                markup.push_str(
                    "<span class=\"timeline-track timeline-unmeasured\"><span>End not recorded</span></span><span class=\"timeline-duration\">—</span></div>",
                );
            }
        }
        markup.push_str("</section>");
    }
    if graph.nodes().len() > selected {
        markup.push_str(&format!(
            "<p class=\"timeline-omitted\">{} additional stage{} omitted from this secondary view; the execution tree remains bounded separately.</p>",
            graph.nodes().len() - selected,
            if graph.nodes().len() - selected == 1 { "" } else { "s" },
        ));
    }
    markup.push_str("</div></details>");
    markup
}

fn compact_usage(usage: &astra_turn_types::ExplainAnalyzeTokenUsageV1) -> String {
    let mut lanes = Vec::new();
    if let Some(value) = usage.fresh_input_tokens {
        lanes.push(format!("in {}", format_tokens(value)));
    }
    if let Some(value) = usage.output_tokens {
        lanes.push(format!("out {}", format_tokens(value)));
    }
    let value = if lanes.is_empty() {
        if usage.cache_read_tokens.is_some() || usage.cache_creation_tokens.is_some() {
            "cache".to_string()
        } else {
            "reported".to_string()
        }
    } else {
        lanes.join(" / ")
    };
    match usage.basis {
        ExplainAnalyzeUsageBasisV1::ProviderExact => value,
        ExplainAnalyzeUsageBasisV1::ProviderPartial => format!("partial {value}"),
        ExplainAnalyzeUsageBasisV1::RuntimeEstimated => format!("~{value}"),
    }
}

fn render_node(
    graph: &ExplainAnalyzeGraphV1,
    node: &ExplainAnalyzeProjectedNodeV1,
    index: usize,
    depth: usize,
    max_duration: u64,
    clock_labels: &HashMap<String, String>,
    verbose: bool,
) -> String {
    let state = node_state(node.terminal_observed, node.outcome);
    let state_class = state_class(state);
    let duration = node
        .duration_ms
        .map(format_ms)
        .unwrap_or_else(|| "not measured".to_string());
    let duration_summary = node
        .duration_ms
        .map(format_ms)
        .unwrap_or_else(|| "—".to_string());
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
    let mut sequence = Vec::new();
    if let Some(round) = round {
        sequence.push(round);
    }
    if let Some(attempt) = attempt {
        sequence.push(attempt);
    }

    let clock_label = clock_labels
        .get(&node.clock_domain_id)
        .map(String::as_str)
        .unwrap_or("clock ?");
    let mut body = format!(
        "<div class=\"stage-body-head\"><span class=\"section-kicker\">Full label</span><div class=\"full-label\">{}</div></div><div class=\"stage-facts\"><div class=\"fact\"><span class=\"fact-label\">kind</span><span class=\"fact-value\">{}</span></div><div class=\"fact\"><span class=\"fact-label\">start</span><span class=\"fact-value\">{}</span></div><div class=\"fact\"><span class=\"fact-label\">clock</span><span class=\"fact-value\">{}</span></div><div class=\"fact\"><span class=\"fact-label\">stage index</span><span class=\"fact-value\">{index}</span></div><div class=\"fact\"><span class=\"fact-label\">duration</span><span class=\"fact-value\">{}</span></div></div>{}",
        escape_html(&node.label, MAX_TEXT_CHARS),
        escape_html(kind_label(node.kind), MAX_TEXT_CHARS),
        format_ms(node.start_elapsed_ms),
        escape_html(clock_label, MAX_TEXT_CHARS),
        escape_html(&duration, MAX_TEXT_CHARS),
        if node.duration_ms.is_some() {
            format!(
                "<div class=\"timeline-inline\" aria-label=\"relative duration\"><span style=\"--bar:{bar}%\"></span></div>"
            )
        } else {
            "<div class=\"timeline-inline timeline-inline-unmeasured\">Duration not measured</div>"
                .to_string()
        },
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

    let indent = depth.min(8) * 12;
    format!(
        "<details class=\"stage stage-row state-{state_class}{}\" style=\"--indent:{indent}px\" data-index=\"{index}\"><summary><span class=\"stage-leading\"><span class=\"stage-toggle\" aria-hidden=\"true\">›</span><span class=\"stage-marker\" aria-hidden=\"true\"></span></span><span class=\"stage-label\"><span class=\"stage-name\">{}</span><span class=\"stage-kind\">{}</span></span><span class=\"stage-duration\">{}</span><span class=\"stage-tokens\">{}</span><span class=\"stage-outcome\"><span class=\"outcome-dot\" aria-hidden=\"true\"></span>{}</span></summary><div class=\"stage-body\">{body}</div></details>",
        if node.conflicted || (node.parent_index.is_none() && node.parent_node_id.is_some()) {
            " conflict"
        } else {
            ""
        },
        escape_html(&node.label, MAX_TEXT_CHARS),
        escape_html(kind_label(node.kind), MAX_TEXT_CHARS),
        escape_html(&duration_summary, MAX_TEXT_CHARS),
        escape_html(
            &node
                .usage
                .as_ref()
                .map(compact_usage)
                .unwrap_or_else(|| "—".to_string()),
            MAX_TEXT_CHARS,
        ),
        escape_html(state, MAX_TEXT_CHARS),
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
        for report in &assembly.edge_memory_selection {
            markup.push_str(&format!(
                "<details><summary>{}</summary>",
                escape_html(&report.summary(), MAX_TEXT_CHARS)
            ));
            for line in report.detail_lines() {
                markup.push_str(&format!(
                    "<p class=\"detail-line\">{}</p>",
                    escape_html(&line, MAX_TEXT_CHARS)
                ));
            }
            markup.push_str("</details>");
        }
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
                "<section class=\"panel alert-panel coverage\"><div class=\"panel-heading\"><div><span class=\"section-kicker\">Bounded view</span><h2>Report bounded for safety</h2></div><span class=\"alert-mark\" aria-hidden=\"true\">!</span></div><p class=\"note\">{} stage detail{} omitted to keep this offline report readable and below 1 MiB. The canonical JSON artifact retains the complete fact stream.</p></section>",
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
        assert!(html.contains("Task outcome and capture completeness are shown separately."));
        assert!(html.contains("Execution tree"));
        assert!(html.contains("Showing 2 of 2 stages"));
        assert!(html.contains("Timeline by clock domain"));
        assert!(html.contains("data-index=\"0\""));
        assert!(!html.contains("class=\"hero\""));
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
        assert!(html.contains("<strong>—</strong><span>turn wall time"));
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
        assert!(html.contains("0ms · instant"));
        assert!(html.contains("timeline-micro"));
        assert!(html.contains("1ms · short"));
        assert!(!html.contains("width:1.00%"));
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

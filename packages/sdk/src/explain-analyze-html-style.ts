// Self-contained styles for the offline Explain Analyze report.
export const EXPLAIN_ANALYZE_HTML_STYLE = `
:root {
  color-scheme: dark;
  --bg: #0c1219;
  --surface: #101923;
  --surface-raised: #141f2b;
  --ink: #eef3fa;
  --muted: #9cafc5;
  --dim: #74869e;
  --line: #2b3a4b;
  --line-soft: #1d2a38;
  --cyan: #59e3c7;
  --blue: #73baff;
  --amber: #edbd61;
  --red: #f1747e;
  --violet: #aaa6ff;
  font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
}
* { box-sizing: border-box; }
html { background: var(--bg); }
body {
  min-width: 320px;
  margin: 0;
  background: var(--bg);
  color: var(--ink);
}
a { color: var(--blue); text-decoration: none; }
a:hover { text-decoration: underline; }
button, input, textarea { font: inherit; }
.shell {
  max-width: 1420px;
  margin: 0 auto;
  padding: 26px 46px 42px;
}
.topline, .report-head, .tree-heading, .tree-actions, .secondary-view > summary, .footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 18px;
}
.topline { margin-bottom: 38px; }
.brand {
  display: flex;
  align-items: center;
  gap: 11px;
  color: var(--ink);
  font-weight: 700;
  letter-spacing: .04em;
}
.brand b { color: var(--dim); font-weight: 400; }
.brand-mark {
  display: grid;
  width: 26px;
  height: 26px;
  place-items: center;
  color: #07131c;
  background: var(--blue);
  clip-path: polygon(50% 0, 100% 100%, 76% 100%, 65% 73%, 35% 73%, 24% 100%, 0 100%);
  font-size: 0;
}
.brand-mark::after { content: "A"; font-size: 13px; font-weight: 900; }
.top-actions { display: flex; gap: 22px; }
.top-actions a { color: var(--blue); }
.report-head { align-items: flex-end; gap: 30px; }
.eyebrow {
  margin: 0 0 8px;
  color: var(--dim);
  font-size: 11px;
  letter-spacing: .12em;
  text-transform: uppercase;
}
.report-head h1 {
  max-width: 900px;
  margin: 0;
  color: var(--ink);
  font-size: clamp(22px, 3vw, 30px);
  font-weight: 650;
  letter-spacing: -.035em;
  line-height: 1.2;
  overflow-wrap: anywhere;
}
.subtitle {
  margin: 8px 0 0;
  color: var(--muted);
  font-size: 12px;
}
.report-result {
  display: flex;
  align-items: baseline;
  flex: 0 0 auto;
  gap: 14px;
  white-space: nowrap;
}
.report-result > strong {
  color: var(--cyan);
  font-size: 20px;
  font-weight: 650;
}
.state {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  color: var(--muted);
  font-size: 12px;
}
.state::before {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: currentColor;
  content: "";
}
.state-complete { color: var(--cyan); }
.state-failed { color: var(--red); }
.state-running { color: var(--amber); }
.report-facts, .token-summary, .wait-summary {
  margin: 7px 0 0;
  color: var(--muted);
  font-size: 12px;
}
.report-facts:first-of-type { margin-top: 24px; }
.report-facts-warning, .wait-summary { color: var(--amber); }
.token-summary { color: var(--cyan); }
.warning {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  margin: 18px 0 0;
  padding: 10px 12px;
  border: 1px solid #755d34;
  border-radius: 4px;
  background: #211d15;
  color: var(--amber);
  font-size: 12px;
}
.warning-icon {
  display: grid;
  flex: 0 0 auto;
  width: 17px;
  height: 17px;
  place-items: center;
  border: 1px solid currentColor;
  border-radius: 50%;
  font-weight: 700;
}
.tree-panel {
  margin-top: 28px;
  border-top: 1px solid var(--line);
  border-bottom: 1px solid var(--line);
}
.tree-heading {
  align-items: flex-end;
  padding: 15px 0 10px;
}
.tree-heading h2, .copy-panel h2 {
  margin: 0;
  color: var(--ink);
  font-size: 13px;
  font-weight: 650;
}
.tree-heading p {
  margin: 4px 0 0;
  color: var(--dim);
  font-size: 11px;
}
.tree-search-hint {
  flex: 0 0 auto;
  color: var(--dim);
  font-size: 11px;
}
.tree-actions {
  justify-content: flex-start;
  padding: 0 0 12px;
  color: var(--dim);
  font-size: 11px;
}
.tree-actions a { color: var(--blue); }
.text-tree { padding: 2px 0 20px; overflow-x: auto; }
.text-tree-domain + .text-tree-domain { margin-top: 18px; }
.text-tree-domain > h3 {
  margin: 0;
  padding: 10px 0 6px;
  color: var(--dim);
  font-size: 11px;
  font-weight: 500;
  letter-spacing: .03em;
}
.text-tree-group, .text-tree-leaf { min-width: min(100%, 1160px); }
.text-tree-group > summary {
  display: block;
  list-style: none;
  cursor: pointer;
  outline: none;
}
.text-tree-group > summary::-webkit-details-marker, .text-tree-group > summary::marker { display: none; content: ""; }
.text-tree-group > summary:focus-visible { outline: 1px solid var(--blue); outline-offset: 2px; }
.tree-row {
  display: flex;
  align-items: baseline;
  min-width: 720px;
  gap: 7px;
  padding: 2px 8px;
  border-radius: 3px;
  line-height: 1.6;
  white-space: normal;
}
.tree-row:hover { background: #162231; }
.tree-guide {
  flex: 0 0 auto;
  color: #91a4bc;
  white-space: pre;
}
.tree-disclosure {
  display: inline-block;
  flex: 0 0 auto;
  width: 13px;
  color: #a8b9ce;
  transition: transform .12s ease;
}
.text-tree-group:not([open]) > summary .tree-disclosure { transform: rotate(-90deg); }
.tree-leaf-mark {
  display: inline-block;
  flex: 0 0 auto;
  width: 6px;
  height: 6px;
  margin: 0 4px;
  border-radius: 50%;
  background: var(--dim);
}
.tree-status-complete .tree-leaf-mark { background: var(--cyan); }
.tree-status-failed .tree-leaf-mark { background: var(--red); }
.tree-status-waiting .tree-leaf-mark, .tree-status-conflicted .tree-leaf-mark { background: var(--amber); }
.tree-status-running .tree-leaf-mark { background: var(--blue); }
.tree-label {
  min-width: 14ch;
  color: var(--ink);
  font-weight: 550;
  overflow-wrap: anywhere;
}
.tree-details {
  margin-left: auto;
  flex: 0 1 52%;
  color: var(--cyan);
  font-size: 12px;
  white-space: normal;
}
.tree-status-failed .tree-details { color: var(--red); }
.tree-status-waiting .tree-details, .tree-status-conflicted .tree-details { color: var(--amber); }
.tree-status-running .tree-details { color: var(--blue); }
.tree-child-count {
  flex: 0 0 auto;
  color: var(--dim);
  font-size: 11px;
}
.tree-row-wait { background: #1d1a13; }
.tree-row-wait:hover { background: #282218; }
.tree-dependencies {
  margin: 0 8px 2px 52px;
  color: var(--violet);
  font-size: 11px;
  overflow-wrap: anywhere;
}
.inline-explanation {
  max-width: 760px;
  margin: 3px 8px 6px 52px;
  color: var(--muted);
  font-size: 11px;
}
.inline-explanation > summary { color: var(--muted); cursor: pointer; }
.inline-explanation p { margin: 4px 0; color: var(--dim); }
.inline-explanation dl {
  display: grid;
  grid-template-columns: minmax(120px, 1fr) minmax(140px, 1fr);
  gap: 2px 14px;
  margin: 4px 0 0;
}
.inline-explanation dt { color: var(--dim); }
.inline-explanation dd { margin: 0; color: var(--muted); }
.text-tree-children { display: block; }
.copy-panel {
  margin-top: 24px;
  padding: 14px 0 0;
  border-top: 1px solid var(--line);
}
.copy-panel textarea {
  display: block;
  width: 100%;
  min-height: 100px;
  margin-top: 10px;
  padding: 10px;
  border: 1px solid var(--line);
  border-radius: 3px;
  resize: vertical;
  outline: none;
  background: var(--surface);
  color: var(--ink);
  line-height: 1.55;
  white-space: pre;
}
.copy-panel textarea:focus { border-color: var(--blue); box-shadow: 0 0 0 2px #73baff33; }
.copy-panel p { margin: 6px 0 0; color: var(--dim); font-size: 11px; }
.secondary-views { margin-top: 26px; border-top: 1px solid var(--line); }
.secondary-view { border-bottom: 1px solid var(--line); }
.secondary-view > summary {
  list-style: none;
  padding: 12px 0;
  cursor: pointer;
  outline: none;
}
.secondary-view > summary::-webkit-details-marker, .secondary-view > summary::marker { display: none; content: ""; }
.secondary-view > summary::before { content: "▸"; color: var(--dim); }
.secondary-view[open] > summary::before { content: "▾"; }
.secondary-view > summary span { margin-right: auto; color: var(--ink); }
.secondary-view > summary small { color: var(--dim); font-size: 11px; }
.secondary-view > summary:focus-visible { outline: 1px solid var(--blue); outline-offset: 2px; }
.coverage-notice {
  margin-top: 15px;
  color: var(--amber);
  font-size: 11px;
}
.empty { padding: 26px 0; color: var(--dim); }
.footer {
  align-items: flex-start;
  margin-top: 22px;
  color: var(--dim);
  font-size: 11px;
}
.footer strong { color: var(--muted); font-weight: 400; }

/* Secondary timeline view. It keeps the measured time axis available without
   making a mini chart compete with the default evidence tree. */
.graph-scroll { overflow: auto; padding: 0 0 18px; }
.timeline { min-width: 850px; }
.axis-row, .graph-row {
  display: grid;
  grid-template-columns: minmax(245px, 330px) minmax(300px, 1fr) 125px 72px;
  gap: 13px;
  align-items: center;
}
.axis-row { min-height: 31px; border-bottom: 1px solid var(--line); }
.axis-track { position: relative; height: 28px; border-bottom: 1px solid var(--line); }
.axis-tick {
  position: absolute;
  bottom: 3px;
  transform: translateX(-50%);
  color: var(--dim);
  font-size: 10px;
  white-space: nowrap;
}
.axis-tick:first-child { transform: none; }
.axis-tick:last-child { transform: translateX(-100%); }
.axis-side { color: var(--dim); font-size: 10px; letter-spacing: .08em; text-transform: uppercase; }
.graph-row { min-height: 42px; padding: 6px 0; border-bottom: 1px solid var(--line-soft); }
.node-copy { min-width: 0; }
.node-titleline { display: flex; align-items: baseline; min-width: 0; gap: 6px; }
.node-title { min-width: 0; color: var(--ink); overflow-wrap: anywhere; }
.node-copy.nested { padding-left: calc(var(--depth, 0) * 20px); }
.node-meta { display: flex; gap: 7px; margin-top: 2px; color: var(--dim); font-size: 10px; }
.node-status { color: var(--muted); }
.kind-wait .node-status, .kind-wait .duration { color: var(--amber); }
.kind-admission .node-title, .kind-admission .duration { color: var(--dim); }
.node-usage, .node-dependencies { color: var(--muted); }
.node-dependencies { margin-top: 2px; font-size: 10px; }
.toggle { color: var(--muted); }
.node-children { display: block; }
.plot { position: relative; height: 12px; border-radius: 2px; background: #182433; }
.bar { position: absolute; top: 2px; height: 8px; min-width: 3px; border-radius: 2px; background: var(--blue); }
.bar.status-complete { background: var(--cyan); }
.bar.status-failed { background: var(--red); }
.bar.status-waiting, .bar.status-conflicted { background: var(--amber); }
.bar.status-running { background: var(--blue); }
.bar.kind-admission { border: 1px dashed var(--dim); background: transparent; }
.bar.kind-wait { background: repeating-linear-gradient(135deg, var(--amber) 0 5px, #4b3c24 5px 10px); }
.interval, .duration { color: var(--muted); font-size: 10px; font-variant-numeric: tabular-nums; white-space: nowrap; }
.interval { text-align: right; }
.duration { color: var(--cyan); text-align: right; font-weight: 600; }
.domain-label { margin: 0; padding: 13px 0 6px; color: var(--dim); font-size: 11px; font-weight: 500; }

/* Secondary graph view. Paths come from the shared deterministic layout. */
.dag-limit { margin: 12px 0; color: var(--amber); font-size: 11px; }
.dag-domain { padding: 15px 0 18px; }
.dag-domain + .dag-domain { border-top: 1px solid var(--line-soft); }
.dag-domain-head { display: flex; justify-content: space-between; gap: 14px; padding-bottom: 10px; color: var(--muted); font-size: 11px; }
.dag-domain-head span { color: var(--dim); }
.dag-key-parent, .dag-key-dependency { display: inline-block; width: 20px; margin: 0 6px; border-top: 1.5px solid #8494aa; vertical-align: middle; }
.dag-key-dependency { border-top-style: dashed; border-color: var(--violet); }
.dag-scroll { overflow: auto; border: 1px solid var(--line-soft); border-radius: 3px; background: #0f1721; }
.dag-canvas { position: relative; min-width: 100%; }
.dag-canvas > svg { position: absolute; inset: 0; pointer-events: none; overflow: visible; }
.dag-edge { fill: none; stroke: #788aa1; stroke-width: 1.5; }
.dag-edge-dependency { stroke: var(--violet); stroke-dasharray: 4 4; }
.dag-card {
  position: absolute;
  display: flex;
  flex-direction: column;
  justify-content: center;
  padding: 10px 12px;
  border: 1px solid #334458;
  border-radius: 4px;
  background: var(--surface-raised);
  color: var(--ink);
  text-decoration: none;
}
.dag-card:hover, .dag-card:focus-visible { z-index: 2; border-color: var(--blue); outline: none; box-shadow: 0 0 0 2px #73baff33; }
.dag-card-head { display: flex; align-items: center; gap: 6px; order: 2; margin-top: 6px; color: var(--muted); font-size: 10px; }
.dag-card-head i { width: 6px; height: 6px; border-radius: 50%; background: var(--dim); }
.dag-card-head b { margin-left: auto; color: var(--cyan); font-weight: 550; }
.dag-status-complete .dag-card-head i { background: var(--cyan); }
.dag-status-failed { border-color: #77404b; background: #21181f; }
.dag-status-failed .dag-card-head i { background: var(--red); }
.dag-status-failed .dag-card-head b { color: var(--red); }
.dag-kind-wait { border-color: #70592f; background: #211d15; }
.dag-kind-wait .dag-card-head i, .dag-kind-wait .dag-card-head b { color: var(--amber); background: var(--amber); }
.dag-kind-wait .dag-card-head b { background: transparent; }
.dag-kind-admission { border-style: dashed; background: #111a24; }
.dag-card-title { order: 0; color: var(--ink); font-size: 12px; font-weight: 550; line-height: 1.45; overflow-wrap: anywhere; }
.dag-card-usage { order: 1; margin-top: 5px; color: var(--muted); font-size: 10px; overflow-wrap: anywhere; }
.dag-inspection { display: none; padding: 14px 0; border-top: 1px solid var(--line-soft); color: var(--muted); font-size: 11px; }
.dag-inspection:target { display: block; }
.dag-inspection h3 { margin: 0 0 4px; color: var(--ink); font-size: 13px; font-weight: 550; }
.dag-inspection p { margin: 5px 0; }
.dag-inspection h4 { color: var(--muted); font-size: 11px; }
.dag-inspection dl { display: grid; grid-template-columns: minmax(130px, 1fr) minmax(130px, 1fr); gap: 3px 12px; }
.dag-inspection dt { color: var(--dim); }
.dag-inspection dd { margin: 0; color: var(--muted); }
.dag-close { float: right; color: var(--blue); }

@media (max-width: 760px) {
  .shell { padding: 20px 18px 32px; }
  .topline { margin-bottom: 28px; }
  .report-head { align-items: flex-start; flex-direction: column; gap: 12px; }
  .report-result { width: 100%; justify-content: space-between; }
  .tree-heading { align-items: flex-start; flex-direction: column; }
  .tree-search-hint { width: 100%; }
  .tree-details { margin-left: 8px; white-space: normal; }
  .tree-row { min-width: 650px; }
  .footer { flex-direction: column; }
  .axis-row, .graph-row { grid-template-columns: 220px minmax(250px, 1fr) 90px 56px; gap: 8px; }
}
@media (prefers-reduced-motion: reduce) {
  .tree-disclosure { transition: none; }
}
`;

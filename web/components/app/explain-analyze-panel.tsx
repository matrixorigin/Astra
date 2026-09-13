"use client";

import { Activity, AlertTriangle, ChevronRight, Download, Pause, Play, RotateCcw } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import type { CSSProperties, ReactNode } from "react";
import {
  explainAnalyzeMaxConcurrency,
  formatMs,
  formatUsageDetail,
  formatUsage,
  reduceExplainAnalyzeEvents,
  renderExplainAnalyzeHtml,
} from "@astra/sdk";
import type { ExplainAnalyzeNodeV1 } from "@astra/sdk";
import { cn } from "@/lib/utils/cn";

const INITIAL_VISIBLE_NODES = 500;
const TOKEN_LANES = [
  ["Fresh input", "fresh_input_tokens"],
  ["Cache read", "cache_read_tokens"],
  ["Cache created", "cache_creation_tokens"],
  ["Output", "output_tokens"],
] as const;

type GraphView = "tree" | "timeline";

type TimelineBudget = { rendered: number; limit: number };

export function ExplainAnalyzePanel({
  events,
  degraded = false,
}: {
  events: readonly unknown[];
  degraded?: boolean;
}) {
  const panelId = useId();
  const [timelineOpen, setTimelineOpen] = useState(true);
  const [graphView, setGraphView] = useState<GraphView>("tree");
  const [visibleCount, setVisibleCount] = useState(INITIAL_VISIBLE_NODES);
  const [expandedNodeIds, setExpandedNodeIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const [collapsedNodeIds, setCollapsedNodeIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const graph = useMemo(() => reduceExplainAnalyzeEvents(events), [events]);
  const clockGroups = useMemo(() => groupByClockDomain(graph.nodes), [graph.nodes]);
  const visibleClockGroups = useMemo(() => {
    let remaining = visibleCount;
    return clockGroups.flatMap(([clockDomainId, nodes], index) => {
      if (remaining <= 0) return [];
      const limit = Math.min(nodes.length, remaining);
      remaining -= limit;
      return [{ clockDomainId, nodes, domainNumber: index + 1, limit }];
    });
  }, [clockGroups, visibleCount]);
  const turnNodes = graph.nodes.filter((node) => node.kind === "turn");
  const finishedTurns = turnNodes.filter((node) => node.durationMs !== undefined);
  const turnTime = finishedTurns.length > 0
    ? formatMs(Math.max(...finishedTurns.map((node) => node.durationMs ?? 0)))
    : turnNodes.length > 0 ? "In progress" : "Not recorded";
  const completedAttempts = graph.nodes.filter(
    (node) => node.kind === "provider_attempt" && node.terminalObserved,
  );
  const slowestRequest = [...completedAttempts]
    .filter((node) => node.durationMs !== undefined)
    .sort((left, right) => (right.durationMs ?? 0) - (left.durationMs ?? 0))[0];
  const maxConcurrency = explainAnalyzeMaxConcurrency(graph);
  const closedClockDomains = useMemo(() => new Set(graph.nodes
    .filter((node) => node.terminalObserved && (node.kind === "turn" || node.kind === "run"))
    .map((node) => node.clockDomainId)), [graph.nodes]);
  const unresolvedTerminalNodes = graph.nodes.some((node) => !node.terminalObserved && closedClockDomains.has(node.clockDomainId));
  const activeCount = graph.nodes.filter((node) => !node.terminalObserved && !closedClockDomains.has(node.clockDomainId)).length;
  const hasActiveNodes = activeCount > 0;
  const latestElapsedByClockDomain = useMemo(() => {
    const latest = new Map<string, number>();
    for (const node of graph.nodes) {
      const observed = Math.max(node.startElapsedMs, node.endElapsedMs ?? node.startElapsedMs);
      latest.set(node.clockDomainId, Math.max(latest.get(node.clockDomainId) ?? 0, observed));
    }
    return latest;
  }, [graph.nodes]);
  const clockAnchors = useRef(new Map<string, { elapsedMs: number; wallMs: number }>());
  const [clockPulse, setClockPulse] = useState(() => performance.now());
  useEffect(() => {
    const wallMs = performance.now();
    for (const [clockDomainId, elapsedMs] of latestElapsedByClockDomain) {
      const previous = clockAnchors.current.get(clockDomainId);
      if (!previous || elapsedMs > previous.elapsedMs) {
        clockAnchors.current.set(clockDomainId, { elapsedMs, wallMs });
      }
    }
  }, [latestElapsedByClockDomain]);
  useEffect(() => {
    if (!hasActiveNodes) return;
    const interval = window.setInterval(() => setClockPulse(performance.now()), 250);
    return () => window.clearInterval(interval);
  }, [hasActiveNodes]);
  const clockNowByDomain = useMemo(() => {
    const wallMs = clockPulse;
    return new Map(
      [...latestElapsedByClockDomain].map(([clockDomainId, elapsedMs]) => {
        const anchor = clockAnchors.current.get(clockDomainId) ?? { elapsedMs, wallMs };
        return [
          clockDomainId,
          closedClockDomains.has(clockDomainId) ? elapsedMs : Math.max(elapsedMs, anchor.elapsedMs + Math.max(0, wallMs - anchor.wallMs)),
        ] as const;
      }),
    );
  }, [clockPulse, latestElapsedByClockDomain, closedClockDomains]);
  const lanes = summarizeTokenLanes(completedAttempts);
  const hasConflict = graph.conflictedNodeIds.length > 0;
  const hasGap = degraded || hasConflict || graph.integrity === "unknown";
  const terminalTurn = [...turnNodes].reverse().find((node) => node.terminalObserved);
  const hasTerminalTurn = terminalTurn !== undefined;
  const showWarning = hasGap || unresolvedTerminalNodes;
  const allExact = completedAttempts.length > 0 && completedAttempts.every(
    (node) => node.usage?.basis === "provider_exact",
  );
  const terminalOutcome = terminalTurn?.outcome;
  const runState = hasGap || unresolvedTerminalNodes
    ? "Incomplete"
    : hasActiveNodes || !hasTerminalTurn
      ? "Live"
      : terminalOutcome === "waiting" || terminalOutcome === "blocked" || terminalOutcome === "deferred"
        ? "Waiting"
        : terminalOutcome === "cancelled"
          ? "Cancelled"
          : terminalOutcome === "failed" || terminalOutcome === "interrupted" || terminalOutcome === "rejected"
            ? terminalOutcome === "interrupted" ? "Interrupted" : "Failed"
            : terminalOutcome === "delegated"
              ? "Delegated"
              : "Complete";

  const downloadHtml = () => {
    const html = renderExplainAnalyzeHtml(events, {
      degraded: showWarning,
      title: "Explain Analyze report",
    });
    const blob = new Blob([html], { type: "text/html;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = "explain-analyze.html";
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 1_000);
  };

  const toggleNode = (nodeId: string, currentlyOpen: boolean) => {
    if (currentlyOpen) {
      setExpandedNodeIds((current) => {
        const next = new Set(current);
        next.delete(nodeId);
        return next;
      });
      setCollapsedNodeIds((current) => new Set(current).add(nodeId));
      return;
    }
    setCollapsedNodeIds((current) => {
      const next = new Set(current);
      next.delete(nodeId);
      return next;
    });
    setExpandedNodeIds((current) => new Set(current).add(nodeId));
  };

  if (graph.nodes.length === 0 && !hasGap) return null;

  return (
    <section
      aria-label="Explain Analyze"
      className="mt-5 overflow-hidden rounded-2xl border border-border bg-surface shadow-[0_8px_26px_rgba(35,52,81,0.08)]"
    >
      <header className="flex flex-wrap items-center justify-between gap-4 border-b border-border px-5 py-4">
        <div className="flex min-w-0 items-start gap-3">
          <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-accent/10 text-accent">
            <Activity className="size-4" aria-hidden="true" />
          </span>
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-2">
              <h2 className="text-sm font-semibold tracking-tight text-text">Explain Analyze</h2>
              <span
                aria-live="polite"
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[10px] font-semibold",
                  runState === "Live" || runState === "Delegated"
                    ? "border-accent/20 bg-accent/5 text-accent"
                    : runState === "Incomplete" || runState === "Waiting"
                      ? "border-warning/25 bg-warning/5 text-warning"
                      : runState === "Failed" || runState === "Interrupted"
                        ? "border-danger/20 bg-danger/5 text-danger"
                        : runState === "Cancelled"
                          ? "border-border bg-bg text-text-muted"
                          : "border-success/20 bg-success/5 text-success",
                )}
              >
                <span className="size-1.5 rounded-full bg-current" aria-hidden="true" />
                {runState}
              </span>
            </div>
            <p className="mt-1 text-xs text-text-muted">
              Timings, dependencies and model usage
            </p>
          </div>
        </div>
        <button
          type="button"
          onClick={downloadHtml}
          disabled={graph.nodes.length === 0}
          className="inline-flex shrink-0 items-center gap-1.5 rounded-control border border-border bg-bg px-3 py-2 text-xs font-medium text-text-secondary transition hover:bg-surface-muted hover:text-text disabled:cursor-not-allowed disabled:opacity-50"
        >
          <Download className="size-3.5" aria-hidden="true" />
          Save graph
        </button>
      </header>

      {showWarning ? (
        <div role="status" aria-live="polite" className="flex items-start gap-2.5 border-b border-warning/20 bg-warning/5 px-5 py-3 text-xs text-warning">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden="true" />
          <p>
            This graph has a delivery gap or unresolved stages. Reconnect to the run to restore facts from its saved event history.
          </p>
        </div>
      ) : null}

      <div className="explain-analyze-summary" aria-label="Execution summary">
        {finishedTurns.length > 0 ? <Metric label="Turn time" value={turnTime} detail="Runtime measured" /> : null}
        {slowestRequest?.durationMs !== undefined ? <Metric label="Slowest model request" value={formatMs(slowestRequest.durationMs)} detail={requestIdentity(slowestRequest)} /> : null}
        {maxConcurrency !== null ? <Metric label="Peak parallel work" value={`${maxConcurrency} at once`} detail="Within the same worker timeline" /> : null}
        {activeCount > 0 ? <Metric label="Active stages" value={String(activeCount)} detail="Includes parent stages" /> : null}
        <span className="explain-analyze-summary-count">{graph.nodes.length} stages</span>
      </div>
      <div className="explain-analyze-token-summary" aria-label="Model token usage">
        {lanes.filter((lane) => lane.value !== null).map((lane) =>
          <span key={lane.label}>{lane.label} <strong>{lane.value}</strong></span>)}
        {lanes.some((lane) => lane.value === null)
          ? <span>{lanes.every((lane) => lane.value === null) ? "Token usage not reported" : "Token usage partly reported"}</span>
          : <span>{allExact ? "Provider reported" : "Includes partial reports or estimates"}</span>}
      </div>

      <section aria-labelledby={`${panelId}-graph-heading`} className="px-4 pb-4 pt-3">
        <button
          type="button"
          aria-expanded={timelineOpen}
          onClick={() => setTimelineOpen((open) => !open)}
          className="flex w-full items-center justify-between gap-3 text-left"
        >
          <span>
            <span id={`${panelId}-graph-heading`} className="block text-sm font-semibold text-text">
              Execution graph
            </span>
            <span className="mt-0.5 block text-[10px] text-text-muted">
              {graphView === "tree" ? "Explore stages, requests and outcomes · ~ marks estimated live time" : "Time runs left to right · overlap means parallel work · ~ marks estimated live time"}
            </span>
          </span>
          <span className="shrink-0 text-xs text-text-muted">
            {graph.nodes.length} stages {timelineOpen ? "· hide" : "· show"}
          </span>
        </button>

        <div role="group" aria-label="Execution graph view" className="explain-analyze-view-switch mt-3">
          {(["tree", "timeline"] as const).map((view) => (
            <button key={view} type="button" aria-pressed={graphView === view}
              onClick={() => setGraphView(view)}>{view === "tree" ? "Tree" : "Timeline"}</button>
          ))}
        </div>
        {timelineOpen ? (
          <div className="mt-3 space-y-3">
            {graphView === "timeline" ? <div aria-label="Graph legend" className="flex flex-wrap items-center gap-x-3 gap-y-2 text-[9px] text-text-muted">
              <StatusLegend colorClass="bg-success" label="Completed" />
              <StatusLegend colorClass="bg-danger" label="Failed" />
              <StatusLegend colorClass="bg-accent" label="In progress" />
              <StatusLegend colorClass="bg-warning" label="Waiting" />
              {graphView === "timeline" ? <>
              <span className="inline-flex items-center gap-1.5">
                <span className="h-2 w-5 rounded-sm border border-dashed border-text-muted/50 bg-text-muted/15" aria-hidden="true" />
                Parent stage
              </span>
              <span className="inline-flex items-center gap-1.5">
                <span className="h-2 w-5 rounded-sm bg-text-secondary" aria-hidden="true" />
                Work stage
              </span>
              </> : null}
            </div> : null}
            {visibleClockGroups.map(({ clockDomainId, nodes, domainNumber, limit }) => (
              <TimelineDomain
                key={clockDomainId}
                graphView={graphView}
                domainNumber={domainNumber}
                nodes={nodes}
                nowElapsedMs={clockNowByDomain.get(clockDomainId) ?? 0}
                isLive={!closedClockDomains.has(clockDomainId)}
                visibleLimit={limit}
                totalGraphSize={graph.nodes.length}
                expandedNodeIds={expandedNodeIds}
                collapsedNodeIds={collapsedNodeIds}
                onToggleNode={toggleNode}
              />
            ))}
            {graph.nodes.length > visibleCount ? (
              <button type="button" onClick={() => setVisibleCount((count) => count + INITIAL_VISIBLE_NODES)}
                className="rounded-control border border-border px-3 py-1.5 text-[10px] font-medium text-text-secondary transition hover:bg-surface-muted">
                Show more stages
              </button>
            ) : null}
            {clockGroups.length > 1 ? (
              <p className="text-[10px] text-text-muted">
                Separate worker timelines use independent clocks; their bars are not compared for overlap.
              </p>
            ) : null}
          </div>
        ) : null}
      </section>
    </section>
  );
}

function Metric({ label, value, detail }: { label: string; value: string; detail: string }) {
  return <span className="explain-analyze-summary-metric" title={detail}>
    <span>{label}</span><strong title={value}>{value}</strong>
  </span>;
}

function StatusLegend({ colorClass, label }: { colorClass: string; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span className={cn("size-2 rounded-full", colorClass)} aria-hidden="true" />
      {label}
    </span>
  );
}

function TimelineDomain({
  graphView,
  domainNumber,
  nodes,
  nowElapsedMs,
  isLive,
  visibleLimit,
  totalGraphSize,
  expandedNodeIds,
  collapsedNodeIds,
  onToggleNode,
}: {
  graphView: GraphView;
  domainNumber: number;
  nodes: readonly ExplainAnalyzeNodeV1[];
  nowElapsedMs: number;
  isLive: boolean;
  visibleLimit: number;
  totalGraphSize: number;
  expandedNodeIds: ReadonlySet<string>;
  collapsedNodeIds: ReadonlySet<string>;
  onToggleNode: (nodeId: string, currentlyOpen: boolean) => void;
}) {
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  const overflowInspectorRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    overflowInspectorRef.current?.scrollIntoView?.({ block: "nearest" });
  }, [selectedNodeId, graphView]);
  const [cursorMs, setCursorMs] = useState<number | null>(null);
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState(1);
  const { nodeById, childrenByParent, roots, recordedDomainEnd, hasOpenNodes } = useMemo(() => {
    const nodeById = new Map(nodes.map((node) => [node.nodeId, node]));
    const childrenByParent = new Map<string, ExplainAnalyzeNodeV1[]>();
    const roots: ExplainAnalyzeNodeV1[] = [];
    let recordedDomainEnd = 1;
    let hasOpenNodes = false;
    for (const node of nodes) {
      recordedDomainEnd = Math.max(recordedDomainEnd, node.endElapsedMs ?? node.startElapsedMs + (node.durationMs ?? 0));
      hasOpenNodes ||= !node.terminalObserved;
      const parent = node.parentNodeId ? nodeById.get(node.parentNodeId) : undefined;
      if (!parent || parent.clockDomainId !== node.clockDomainId) {
        roots.push(node);
        continue;
      }
      const children = childrenByParent.get(parent.nodeId) ?? [];
      children.push(node);
      childrenByParent.set(parent.nodeId, children);
    }
    const connected = new Set<string>();
    const markConnected = (root: ExplainAnalyzeNodeV1) => {
      const pending = [root];
      while (pending.length > 0) {
        const node = pending.pop()!;
        if (connected.has(node.nodeId)) continue;
        connected.add(node.nodeId);
        pending.push(...(childrenByParent.get(node.nodeId) ?? []));
      }
    };
    roots.forEach(markConnected);
    // Keep malformed cycles visible without promoting deliberately collapsed children.
    for (const node of nodes) {
      if (connected.has(node.nodeId)) continue;
      roots.push(node);
      markConnected(node);
    }
    return { nodeById, childrenByParent, roots, recordedDomainEnd, hasOpenNodes };
  }, [nodes]);
  const hasActiveNodes = isLive && hasOpenNodes;
  const domainEnd = hasActiveNodes ? Math.max(recordedDomainEnd, nowElapsedMs) : recordedDomainEnd;
  const position = Math.min(cursorMs ?? domainEnd, domainEnd);
  const selected = selectedNodeId ? nodeById.get(selectedNodeId) : undefined;
  const replaying = graphView === "timeline" && playing && position < domainEnd;
  useEffect(() => {
    if (!replaying) return;
    let previous = performance.now();
    const timer = window.setInterval(() => {
      const now = performance.now();
      const delta = (now - previous) * speed;
      previous = now;
      setCursorMs((current) => Math.min(domainEnd, (current ?? 0) + delta));
    }, 50);
    return () => window.clearInterval(timer);
  }, [replaying, speed, domainEnd]);
  // Replay is a visual inspection cursor over recorded intervals, never a
  // second graph reducer or a source of terminal duration/usage facts.
  const inspect = (nodeId: string) => {
    setSelectedNodeId(nodeId);
    const seen = new Set<string>();
    let parent = nodeById.get(nodeId)?.parentNodeId;
    while (parent && !seen.has(parent)) {
      seen.add(parent);
      onToggleNode(parent, false);
      parent = nodeById.get(parent)?.parentNodeId;
    }
  };
  const ticks = [0, 25, 50, 75, 100];
  const budget: TimelineBudget = { rendered: 0, limit: visibleLimit };
  const visited = new Set<string>();
  const inspector = selected ? (
        <aside aria-label={`Stage details: ${selected.label}`} className="explain-analyze-inspector mt-3 rounded-xl border border-accent/20 bg-surface p-4">
          <div className="flex items-start justify-between gap-3">
            <div><p className="text-[9px] uppercase tracking-widest text-text-muted">Recorded stage</p>
              <h4 className="mt-1 text-sm font-semibold text-text">{selected.label}</h4></div>
            <button type="button" aria-label="Close stage details" className="text-xs text-text-muted" onClick={() => setSelectedNodeId(null)}>Close</button>
          </div>
          <dl className="mt-3 grid grid-cols-2 gap-3 text-xs sm:grid-cols-4">
            <div><dt className="text-text-muted">Start</dt><dd>{formatMs(selected.startElapsedMs)}</dd></div>
            <div><dt className="text-text-muted">End</dt><dd>{selected.endElapsedMs === undefined ? "Not recorded" : formatMs(selected.endElapsedMs)}</dd></div>
            <div><dt className="text-text-muted">Measured duration</dt><dd>{selected.durationMs === undefined ? "Not recorded" : formatMs(selected.durationMs)}</dd></div>
            <div><dt className="text-text-muted">Outcome</dt><dd>{!selected.terminalObserved && !isLive ? "End not recorded" : nodeStatus(selected).label}</dd></div>
          </dl>
          {selected.parentNodeId ? <p className="mt-3 text-xs text-text-muted">Parent: {nodeById.get(selected.parentNodeId)?.label ?? selected.parentNodeId}</p> : null}
          {selected.dependencyNodeIds.length > 0 ? <div className="mt-3 text-xs text-text-muted">Depends on: {selected.dependencyNodeIds.map((id) =>
            nodeById.has(id) ? <button key={id} type="button" className="ml-2 text-accent underline" onClick={() => inspect(id)}>{nodeById.get(id)?.label}</button>
              : <span key={id} className="ml-2">{id} (outside this timeline)</span>)}</div> : null}
          {selected.kind === "provider_attempt" ? <p className="mt-3 text-xs text-text-secondary">{requestIdentity(selected)} · {selected.usage ? formatUsageDetail(selected.usage) : "Token usage not reported"}</p> : null}
        </aside>
      ) : null;
  const rows = roots.map((node) => renderTimelineNode({
    node,
    graphView,
    inspector,
    selectedNodeId,
    onSelectNode: inspect,
    cursorMs,
    domainEnd,
    nowElapsedMs,
    isLive,
    nodeById,
    childrenByParent,
    visited,
    budget,
    defaultOpen: totalGraphSize <= 250 || node.kind === "turn" || node.kind === "run",
    expandedNodeIds,
    collapsedNodeIds,
    onToggleNode,
    depth: 0,
  }));

  return (
    <div className={cn("explain-analyze-domain min-w-0 rounded-xl border border-border bg-bg/40 p-3", graphView === "tree" && "explain-analyze-tree-view")}>
      <div className="mb-3 flex justify-between text-[9px] font-medium uppercase tracking-wide text-text-muted">
        <span>{graphView === "tree" ? `Execution ${domainNumber}` : `Timeline ${domainNumber}`}</span>
        <span>{hasActiveNodes ? "~" : ""}{formatMs(domainEnd)} {hasActiveNodes ? "estimated live extent" : "recorded extent"}</span>
      </div>
      {selected && !visited.has(selected.nodeId) ? (
        <div ref={overflowInspectorRef}>
          <p className="text-xs text-text-muted">Selected stage is outside the visible rows. Its recorded details are shown here.</p>
          {inspector}
        </div>
      ) : null}
      <div className="overflow-x-auto pb-1">
        <div className={graphView === "timeline" ? "min-w-[740px]" : "min-w-0"}>
          {graphView === "timeline" ? <div className="grid grid-cols-[minmax(185px,240px)_minmax(250px,1fr)_90px_62px] items-end gap-3 border-b border-border pb-2 text-[9px] text-text-muted">
            <span>Stage</span>
            <div className="relative h-5 border-b border-border">
              {ticks.map((percent) => (
                <span
                  key={percent}
                  className={cn(
                    "absolute bottom-0 translate-x-[-50%] whitespace-nowrap pb-1 tabular-nums",
                    percent === 0 && "translate-x-0",
                    percent === 100 && "translate-x-[-100%]",
                  )}
                  style={{ left: `${percent}%` }}
                >
                  {formatMs((domainEnd * percent) / 100)}
                </span>
              ))}
            </div>
            <span className="text-right">Interval</span>
            <span className="text-right">Time</span>
          </div> : <div className="explain-analyze-tree-columns explain-analyze-column-head"><span>Execution stages</span><span>Tokens</span><span>Outcome</span><span className="text-right">Duration</span></div>}
          <div className="relative">
            {graphView === "timeline" ? <div className="explain-analyze-grid pointer-events-none absolute inset-0" aria-hidden="true">
              <div />
              <div className="relative">
                <div className="explain-analyze-elapsed absolute inset-y-0 left-0" style={{ width: `${position / domainEnd * 100}%` }} />
                <div className="explain-analyze-playhead absolute inset-y-0" style={{ left: `${position / domainEnd * 100}%` }} />
              </div>
            </div> : null}
            {rows}
          </div>
        </div>
      </div>
      {graphView === "timeline" ? <><div className="mt-3 flex flex-wrap items-center gap-3 border-t border-border pt-3">
        <button type="button" aria-label={replaying ? `Pause timeline ${domainNumber}` : `Play timeline ${domainNumber}`}
          className="explain-analyze-control" onClick={() => {
            if (replaying) { setPlaying(false); return; }
            if (cursorMs === null || position >= domainEnd) setCursorMs(0);
            setPlaying(true);
          }}>
          {replaying ? <Pause className="size-3.5" /> : <Play className="size-3.5" />}
        </button>
        <button type="button" aria-label={`Reset timeline ${domainNumber}`} className="explain-analyze-control"
          onClick={() => { setPlaying(false); setCursorMs(0); }}><RotateCcw className="size-3.5" /></button>
        <input type="range" aria-label={`Timeline ${domainNumber} position`} min={0} max={domainEnd} step="any"
          value={position} aria-valuetext={`${formatMs(position)} of ${formatMs(domainEnd)}`}
          className="min-w-24 flex-1 accent-accent"
          onChange={(event) => { setPlaying(false); setCursorMs(Number(event.target.value)); }} />
        <span className="text-[10px] tabular-nums text-text-secondary">{formatMs(position)} / {formatMs(domainEnd)}</span>
        <button type="button" aria-label={`Timeline ${domainNumber} playback speed`} className="explain-analyze-control"
          onClick={() => setSpeed((current) => current === 1 ? 2 : current === 2 ? 0.5 : 1)}>{speed}×</button>
        <button type="button" className="text-[10px] font-medium text-accent"
          onClick={() => { setPlaying(false); setCursorMs(null); }}>{hasActiveNodes ? "Follow live" : "Show full record"}</button>
      </div>
      <p className="mt-2 text-[10px] text-text-muted">
        {cursorMs === null ? "Select a bar to inspect its recorded facts." : "Playback cursor · dimmed stages start later. Details show recorded facts, not a historical event snapshot."}
      </p></> : <p className="mt-3 text-xs text-text-muted">Select a stage to inspect its timing, outcome and reported usage.</p>}

    </div>
  );
}

function renderTimelineNode({
  node,
  graphView,
  inspector,
  selectedNodeId,
  onSelectNode,
  cursorMs,
  domainEnd,
  nowElapsedMs,
  isLive,
  nodeById,
  childrenByParent,
  visited,
  budget,
  defaultOpen,
  expandedNodeIds,
  collapsedNodeIds,
  onToggleNode,
  depth,
}: {
  node: ExplainAnalyzeNodeV1;
  graphView: GraphView;
  inspector: ReactNode;
  selectedNodeId: string | null;
  onSelectNode: (nodeId: string) => void;
  cursorMs: number | null;
  domainEnd: number;
  nowElapsedMs: number;
  isLive: boolean;
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>;
  childrenByParent: ReadonlyMap<string, readonly ExplainAnalyzeNodeV1[]>;
  visited: Set<string>;
  budget: TimelineBudget;
  defaultOpen: boolean;
  expandedNodeIds: ReadonlySet<string>;
  collapsedNodeIds: ReadonlySet<string>;
  onToggleNode: (nodeId: string, currentlyOpen: boolean) => void;
  depth: number;
}): ReactNode {
  if (visited.has(node.nodeId) || budget.rendered >= budget.limit) return null;
  visited.add(node.nodeId);
  budget.rendered += 1;
  const children = childrenByParent.get(node.nodeId) ?? [];
  const isOpen = children.length > 0 && (
    expandedNodeIds.has(node.nodeId) ||
    (!collapsedNodeIds.has(node.nodeId) && defaultOpen)
  );
  const missingEnd = !node.terminalObserved && !isLive;
  const canEstimate = !node.terminalObserved && isLive;
  const status = missingEnd
    ? { label: "End not recorded", textClass: "text-warning", dotClass: "bg-warning", barClass: "bg-warning" }
    : nodeStatus(node);
  const endLabel = node.endElapsedMs !== undefined ? formatMs(node.endElapsedMs) : !canEstimate ? "Unknown" : `~${formatMs(nowElapsedMs)}`;
  const durationLabel = node.durationMs !== undefined ? formatMs(node.durationMs) : !canEstimate ? "Unknown" : `~${formatMs(Math.max(0, nowElapsedMs - node.startElapsedMs))}`;
  const numericEnd = node.endElapsedMs ?? (canEstimate ? Math.max(node.startElapsedMs, nowElapsedMs) : node.startElapsedMs);
  const left = (Math.max(0, Math.min(node.startElapsedMs, domainEnd)) / domainEnd) * 100;
  const width = Math.max(((Math.max(node.startElapsedMs, numericEnd) - node.startElapsedMs) / domainEnd) * 100, 0.5);
  const dependencies = node.dependencyNodeIds.slice(0, 3).map(
    (dependencyId) => nodeById.get(dependencyId)?.label ?? dependencyId,
  );
  const request = node.kind === "provider_attempt" ? requestIdentity(node) : "";
  const usage = node.usage ? formatUsage(node.usage) : "";
  const treeDepth = Math.min(depth, 8);
  const indent = treeDepth * (graphView === "tree" ? 20 : 12);

  return (
    <div key={node.nodeId} className="explain-analyze-tree-node">
      <div className={cn(
        "explain-analyze-lane relative items-center border-b border-border/60",
        graphView === "tree" ? "explain-analyze-tree-columns" : "explain-analyze-grid",
        graphView === "tree" && canEstimate && "explain-analyze-tree-active",
        selectedNodeId === node.nodeId && "explain-analyze-lane-selected",
        graphView === "timeline" && cursorMs !== null && node.startElapsedMs > cursorMs && "explain-analyze-lane-future",
        children.length > 0 && "explain-analyze-group-row",
      )}>
        <div
          className={cn(
            "explain-analyze-node-copy flex min-w-0 items-start gap-2",
            depth > 0 && "explain-analyze-node-copy-nested",
          )}
          style={{
            paddingLeft: `${indent}px`,
            "--explain-tree-indent": `${indent}px`,
          } as CSSProperties}
        >
          {children.length > 0 ? (
            <button
              type="button"
              aria-label={`${isOpen ? "Collapse" : "Expand"} ${node.label}`}
              aria-expanded={isOpen}
              onClick={() => onToggleNode(node.nodeId, isOpen)}
              className="mt-0.5 grid size-4 shrink-0 place-items-center rounded border border-border bg-bg text-text-muted transition hover:bg-surface-muted"
            >
              <ChevronRight className={cn("size-3 transition-transform", isOpen && "rotate-90")} aria-hidden="true" />
            </button>
          ) : (
            <span className="mt-0.5 grid size-4 shrink-0 place-items-center" aria-hidden="true"><span className={cn("size-1.5 rounded-full", status.dotClass)} /></span>
          )}
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5">
              {graphView === "tree" ? <button type="button" className="explain-analyze-stage-title"
                aria-label={`Inspect ${node.label}, ${durationLabel}, ${status.label}`}
                aria-pressed={selectedNodeId === node.nodeId} onClick={() => onSelectNode(node.nodeId)}>
                {node.label}
              </button> : <span className="truncate text-[11px] font-semibold text-text" title={node.label}>{node.label}</span>}
              {request ? (
                <span className="shrink-0 rounded border border-border bg-bg px-1 py-px text-[8px] font-medium text-text-muted">
                  {request}
                </span>
              ) : null}
              {children.length > 0 ? (
                <span className="shrink-0 rounded-full bg-surface-muted px-1.5 py-0.5 text-[8px] font-medium text-text-muted">
                  {children.length} stages
                </span>
              ) : null}
            </div>
            {graphView === "timeline" ? <div className="mt-0.5 flex min-w-0 items-center gap-1.5 overflow-hidden whitespace-nowrap text-[9px] text-text-muted">
              <span className={cn("shrink-0 font-medium", status.textClass)}>{status.label}</span>
              {usage ? (
                <span
                  className="truncate border-l border-border pl-1.5"
                  title={node.usage ? formatUsageDetail(node.usage) : usage}
                >
                  {usage}
                </span>
              ) : null}
            </div> : null}
            {graphView === "timeline" && dependencies.length > 0 ? (
              <p className="mt-0.5 truncate text-[8px] text-text-muted" title={dependencies.join(", ")}>
                After {dependencies.map((label) => `“${label}”`).join(", ")}
                {node.dependencyNodeIds.length > dependencies.length
                  ? ` +${node.dependencyNodeIds.length - dependencies.length}`
                  : ""}
              </p>
            ) : null}
          </div>
        </div>
        {graphView === "tree" ? <>
          <span className="explain-analyze-tree-usage" title={node.usage ? formatUsageDetail(node.usage) : undefined}>
            {node.usage ? <>
              <span>{[
                node.usage.fresh_input_tokens !== undefined ? `in ${node.usage.fresh_input_tokens.toLocaleString()}` : null,
                node.usage.output_tokens !== undefined ? `out ${node.usage.output_tokens.toLocaleString()}` : null,
              ].filter(Boolean).join(" · ")}</span>
              <span>{[
                node.usage.cache_read_tokens !== undefined ? `cache ${node.usage.cache_read_tokens.toLocaleString()}` : null,
                node.usage.cache_creation_tokens ? `write ${node.usage.cache_creation_tokens.toLocaleString()}` : null,
                node.usage.basis === "runtime_estimated" ? "estimated" : node.usage.basis === "provider_partial" ? "partial" : null,
              ].filter(Boolean).join(" · ")}</span>
            </> : node.kind === "provider_attempt" ? "Unreported" : ""}
          </span>
          <span className={cn("explain-analyze-tree-status", status.textClass)}>{status.label}</span>
        </> : null}
        {graphView === "timeline" ? <><div
          className="relative h-8 rounded"
          style={{
            backgroundImage:
              "linear-gradient(90deg, transparent 24.8%, rgba(120,130,150,.22) 25%, transparent 25.2%, transparent 49.8%, rgba(120,130,150,.22) 50%, transparent 50.2%, transparent 74.8%, rgba(120,130,150,.22) 75%, transparent 75.2%)",
          }}
        >
          <button
            type="button"
          aria-label={`Inspect ${node.label}${children.length > 0 ? `, parent stage with ${children.length} nested ${children.length === 1 ? "stage" : "stages"}` : ", work stage"}, ${formatMs(node.startElapsedMs)} to ${endLabel}, ${durationLabel}${!node.terminalObserved && isLive ? " estimated elapsed so far" : ""}, ${status.label}`}
            aria-pressed={selectedNodeId === node.nodeId}
            onClick={() => onSelectNode(node.nodeId)}
            className={cn(
              "explain-analyze-bar absolute top-1 h-6 min-w-[4px] rounded-md",
              status.barClass,
              children.length > 0 && "explain-analyze-parent-bar",
              children.length === 0 && !node.terminalObserved && isLive && "explain-analyze-bar-active",
            )}
            style={{ left: `${left}%`, width: `${width}%` }}
           />
        </div>
        <span className="text-right text-[9px] tabular-nums text-text-muted">
          {formatMs(node.startElapsedMs)}–{endLabel}
        </span></> : null}
        <span className="text-right text-[10px] font-semibold tabular-nums text-text-secondary">
          {durationLabel}
        </span>
      </div>
      {selectedNodeId === node.nodeId ? inspector : null}
      {isOpen && children.length > 0 ? (
        <div
          className="explain-analyze-tree-children"
          style={{ "--explain-tree-parent-indent": `${indent}px` } as CSSProperties}
        >
          {children.map((child) => renderTimelineNode({
            node: child,
            graphView,
            inspector,
            selectedNodeId,
            onSelectNode,
            cursorMs,
            domainEnd,
            nowElapsedMs,
            isLive,
            nodeById,
            childrenByParent,
            visited,
            budget,
            defaultOpen: children.length < 250,
            expandedNodeIds,
            collapsedNodeIds,
            onToggleNode,
            depth: depth + 1,
          }))}
        </div>
      ) : null}
    </div>
  );
}

function groupByClockDomain(nodes: readonly ExplainAnalyzeNodeV1[]) {
  const groups = new Map<string, ExplainAnalyzeNodeV1[]>();
  for (const node of nodes) {
    const group = groups.get(node.clockDomainId) ?? [];
    group.push(node);
    groups.set(node.clockDomainId, group);
  }
  return [...groups.entries()];
}

function summarizeTokenLanes(nodes: readonly ExplainAnalyzeNodeV1[]) {
  return TOKEN_LANES.map(([label, key]) => {
    const values = nodes.map((node) => node.usage?.[key]);
    const total = values.reduce<number>((sum, lane) => sum + (lane ?? 0), 0);
    const value = nodes.length > 0 && values.every((lane) => lane !== undefined)
      ? Number.isSafeInteger(total) ? total.toLocaleString() : "Exceeds precise range"
      : null;
    return { label, value };
  });
}

function requestIdentity(node: ExplainAnalyzeNodeV1) {
  const round = node.roundIndex === undefined ? null : node.roundIndex + 1;
  const attempt = node.attemptIndex === undefined ? null : node.attemptIndex + 1;
  if (round === null && attempt === null) return "Model request";
  if (round !== null && attempt !== null) return `Round ${round} · request ${attempt}`;
  return round !== null ? `Round ${round}` : `Request ${attempt}`;
}

function nodeStatus(node: ExplainAnalyzeNodeV1) {
  if (node.conflicted) {
    return { label: "Conflicting facts", textClass: "text-warning", dotClass: "bg-warning", barClass: "bg-warning" };
  }
  if (!node.terminalObserved) {
    return { label: "In progress", textClass: "text-accent", dotClass: "bg-accent", barClass: "bg-accent" };
  }
  if (node.outcome === "failed" || node.outcome === "interrupted") {
    return { label: node.outcome === "failed" ? "Failed" : "Interrupted", textClass: "text-danger", dotClass: "bg-danger", barClass: "bg-danger" };
  }
  if (node.outcome === "waiting" || node.outcome === "blocked" || node.outcome === "deferred") {
    return { label: node.outcome === "waiting" ? "Waiting" : "Blocked", textClass: "text-warning", dotClass: "bg-warning", barClass: "bg-warning" };
  }
  if (node.outcome === "cancelled") {
    return { label: "Cancelled", textClass: "text-text-muted", dotClass: "bg-text-muted", barClass: "bg-text-muted" };
  }
  if (node.outcome === "completed" || node.outcome === "succeeded" || node.outcome === "resolved") {
    return { label: "Completed", textClass: "text-success", dotClass: "bg-success", barClass: "bg-success" };
  }
  if (node.outcome === "rejected") {
    return { label: "Rejected", textClass: "text-danger", dotClass: "bg-danger", barClass: "bg-danger" };
  }
  const labels = {
    reused: "Reused", suppressed: "Skipped", fallback: "Fallback used",
    unavailable: "Unavailable", delegated: "Delegated",
  } as const;
  const label = node.outcome && node.outcome in labels
    ? labels[node.outcome as keyof typeof labels] : "Outcome not recorded";
  return { label, textClass: "text-text-muted", dotClass: "bg-text-muted", barClass: "bg-text-muted" };
}

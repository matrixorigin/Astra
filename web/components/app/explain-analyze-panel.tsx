"use client";

import { AlertTriangle, ChevronRight, Clock3, Cpu, Download, GitBranch, Layers, Pause, Play, RotateCcw, Wrench } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import type { CSSProperties, KeyboardEvent, ReactNode } from "react";
import {
  explainAnalyzeMaxConcurrency,
  explainAnalyzeTurnOutcome,
  explainAnalyzeCoverageGapLabel,
  explainAnalyzeContextSections,
  formatExplainAnalyzeContext,
  formatMs,
  formatUsageDetail,
  formatUsage,
  layoutExplainAnalyzeGraph,
  reduceExplainAnalyzeEvents,
  renderExplainAnalyzeHtml,
  renderExplainAnalyzeText,
} from "@astra/sdk";
import type {
  ExplainAnalyzeLayoutDomainV1,
  ExplainAnalyzeLayoutNodeV1,
  ExplainAnalyzeNodeV1,
} from "@astra/sdk";
import { cn } from "@/lib/utils/cn";

const INITIAL_VISIBLE_NODES = 500;
const TOKEN_LANES = [
  ["Fresh input", "fresh_input_tokens"],
  ["Cache read", "cache_read_tokens"],
  ["Cache created", "cache_creation_tokens"],
  ["Output", "output_tokens"],
] as const;

type GraphView = "graph" | "tree" | "timeline";

type TimelineBudget = { rendered: number; limit: number };

export function ExplainAnalyzePanel({
  events,
  degraded = false,
  live = false,
}: {
  events: readonly unknown[];
  degraded?: boolean;
  live?: boolean;
}) {
  const panelId = useId();
  const [timelineOpen, setTimelineOpen] = useState(true);
  const [graphView, setGraphView] = useState<GraphView>("tree");
  const [search, setSearch] = useState("");
  const [copyStatus, setCopyStatus] = useState("");
  const [visibleCount, setVisibleCount] = useState(INITIAL_VISIBLE_NODES);
  const [expandedNodeIds, setExpandedNodeIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const [collapsedNodeIds, setCollapsedNodeIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const graph = useMemo(() => reduceExplainAnalyzeEvents(events), [events]);
  const clockGroups = useMemo(() => groupByClockDomain(graph.nodes), [graph.nodes]);
  const searchMatches = useMemo(() => {
    const query = search.trim().toLocaleLowerCase();
    if (!query || graphView !== "tree") return null;
    const nodeById = new Map(graph.nodes.map((node) => [node.nodeId, node]));
    const included = new Set<string>();
    let matches = 0;
    for (const node of graph.nodes) {
      if (!node.label.toLocaleLowerCase().includes(query)) continue;
      matches++;
      let current: ExplainAnalyzeNodeV1 | undefined = node;
      while (current && !included.has(current.nodeId)) {
        included.add(current.nodeId);
        current = current.parentNodeId ? nodeById.get(current.parentNodeId) : undefined;
      }
    }
    return { included, matches };
  }, [graph.nodes, search, graphView]);
  const visibleClockGroups = useMemo(() => {
    let remaining = visibleCount;
    return clockGroups.flatMap(([clockDomainId, allNodes], index) => {
      const nodes = searchMatches ? allNodes.filter((node) => searchMatches.included.has(node.nodeId)) : allNodes;
      if (nodes.length === 0) return [];
      if (remaining <= 0) return [];
      const limit = Math.min(nodes.length, remaining);
      remaining -= limit;
      return [{ clockDomainId, nodes, domainNumber: index + 1, limit }];
    });
  }, [clockGroups, visibleCount, searchMatches]);
  const turnNodes = graph.nodes.filter((node) => node.kind === "turn");
  const finishedTurns = turnNodes.filter((node) => node.durationMs !== undefined);
  const turnTime = finishedTurns.length > 0
    ? formatMs(Math.max(...finishedTurns.map((node) => node.durationMs ?? 0)))
    : live && turnNodes.length > 0 ? "In progress" : "Not recorded";
  const observedAttempts = graph.nodes.filter((node) => node.kind === "provider_attempt");
  const completedAttempts = graph.nodes.filter(
    (node) => node.kind === "provider_attempt" && node.terminalObserved && !node.conflicted,
  );
  const slowestRequest = [...completedAttempts]
    .filter((node) => node.durationMs !== undefined)
    .sort((left, right) => (right.durationMs ?? 0) - (left.durationMs ?? 0))[0];
  const maxConcurrency = explainAnalyzeMaxConcurrency(graph);
  const measuredWaitMs = graph.nodes
    .filter((node) => node.kind === "wait" && node.terminalObserved && !node.conflicted && node.durationMs !== undefined)
    .reduce((total, node) => total + (node.durationMs ?? 0), 0);
  const openWaitCount = graph.nodes.filter((node) => node.kind === "wait" && !node.terminalObserved).length;
  const missingEndNodeIds = useMemo(() => new Set(graph.diagnostics
    .filter((item) => item.code === "unresolved_terminal_node").flatMap((item) => item.nodeId ? [item.nodeId] : [])), [graph.diagnostics]);
  const closedClockDomains = useMemo(() => new Set(clockGroups
    .filter(([, nodes]) => nodes.every((node) => node.terminalObserved || missingEndNodeIds.has(node.nodeId)))
    .map(([clock]) => clock)), [clockGroups, missingEndNodeIds]);
  const unresolvedTerminalNodes = missingEndNodeIds.size > 0;
  const activeCount = graph.nodes.filter((node) => !node.terminalObserved && !missingEndNodeIds.has(node.nodeId)).length;
  const hasActiveNodes = live && activeCount > 0;
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
  const reportedAttempts = completedAttempts.filter((node) => node.usage !== undefined);
  const lanes = summarizeTokenLanes(observedAttempts);
  const hasConflict = graph.conflictedNodeIds.length > 0;
  const hasGap = degraded || hasConflict || graph.integrity === "unknown";
  const terminalTurn = [...turnNodes].reverse().find((node) => node.terminalObserved);
  const hasTerminalTurn = terminalTurn !== undefined;
  const showWarning = hasGap || unresolvedTerminalNodes;
  const allExact = !hasGap && observedAttempts.length === completedAttempts.length && completedAttempts.length > 0 && completedAttempts.every(
    (node) => node.usage?.basis === "provider_exact",
  );
  const runState = hasGap || unresolvedTerminalNodes
    ? "Incomplete"
    : activeCount > 0 || !hasTerminalTurn
      ? live ? "Live" : "Snapshot"
      : explainAnalyzeTurnOutcome(graph.nodes) ?? "Snapshot";

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

  const copyTree = async () => {
    try {
      await navigator.clipboard.writeText(renderExplainAnalyzeText(events, { degraded }));
      setCopyStatus("Tree copied");
    } catch {
      setCopyStatus("Could not access the clipboard. Save the report instead.");
    }
  };
  const setAllExpanded = (open: boolean) => {
    const parents = new Set(graph.nodes.flatMap((node) => node.parentNodeId ? [node.parentNodeId] : []));
    setExpandedNodeIds(open ? parents : new Set());
    setCollapsedNodeIds(open ? new Set() : parents);
  };

  if (graph.nodes.length === 0 && !hasGap) return null;

  return (
    <section
      aria-label="Explain Analyze"
      className="explain-analyze-panel mt-5 overflow-hidden rounded-xl border border-border bg-surface"
    >
      <header className="flex flex-wrap items-center justify-between gap-3 px-5 py-3">
        <div className="flex min-w-0 items-start gap-3">

          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-2">
              <h2 className="text-sm font-semibold tracking-tight text-text">{turnNodes.length === 1 ? turnNodes[0].label : "Explain Analyze"}</h2>
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
              Explain Analyze · recorded timings and model usage
            </p>
          </div>
        </div>
        <div className="flex items-center gap-2">
        <button type="button" className="explain-analyze-text-action" onClick={() => { void copyTree(); }}>Copy tree</button>
        <button
          type="button"
          onClick={downloadHtml}
          disabled={graph.nodes.length === 0}
          className="inline-flex shrink-0 items-center gap-1.5 rounded-control border border-border bg-bg px-3 py-2 text-xs font-medium text-text-secondary transition hover:bg-surface-muted hover:text-text disabled:cursor-not-allowed disabled:opacity-50"
        >
          <Download className="size-3.5" aria-hidden="true" />
          Save graph
        </button>
        </div>
      </header>
      {copyStatus ? <p role="status" className="px-5 pb-2 text-xs text-text-secondary">{copyStatus}</p> : null}

      {showWarning ? (
        <div role="status" aria-live="polite" className="flex items-start gap-2.5 border-b border-warning/20 bg-warning/5 px-5 py-3 text-xs text-warning">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden="true" />
          <p>
            This graph has a delivery gap or unresolved stages. Reconnect to the run to restore facts from its saved event history.
          </p>
        </div>
      ) : null}

      {graph.coverageGaps.length > 0 ? (
        <div role="note" aria-label="Explain Analyze coverage gaps" className="flex flex-wrap items-center gap-x-2 gap-y-1 border-b border-border bg-surface-muted/40 px-5 py-2.5 text-[11px] text-text-muted">
          <span className="font-medium text-warning">Partial capture</span>
          <span>Not timed separately:</span>
          <span>{graph.coverageGaps.map(explainAnalyzeCoverageGapLabel).join(" · ")}</span>
        </div>
      ) : null}

      <div className="explain-analyze-summary" aria-label="Execution summary">
        {finishedTurns.length > 0 ? <Metric label={turnNodes.length > 1 ? "Longest turn" : "Turn time"} value={turnTime} detail="Runtime measured" /> : null}
        {slowestRequest?.durationMs !== undefined ? <Metric label="Slowest model request" value={formatMs(slowestRequest.durationMs)} detail={requestIdentity(slowestRequest)} /> : null}
        {maxConcurrency !== null ? <Metric
          label={graph.coverageGaps.length > 0 ? "Observed overlap" : "Peak parallel work"}
          value={graph.coverageGaps.length > 0 ? `At least ${maxConcurrency} at once` : `${maxConcurrency} at once`}
          detail={graph.coverageGaps.length > 0 ? "Lower bound from recorded spans, evaluated per clock domain" : "Measured within each worker timeline"}
        /> : null}
        {measuredWaitMs > 0 ? <Metric
          label="Measured wait time"
          value={formatMs(measuredWaitMs)}
          detail="Sum of measured wait intervals; overlapping waits may add. Separate I/O time is not inferred."
        /> : null}
        {openWaitCount > 0 ? <span className="text-[10px] text-warning">{openWaitCount} wait {openWaitCount === 1 ? "interval" : "intervals"} still open</span> : null}
        {live && activeCount > 0 ? <Metric label="Active stages" value={String(activeCount)} detail="Includes parent stages" /> : null}
        <span className="explain-analyze-summary-count">{graph.nodes.length} stages</span>
      </div>
      <div className="explain-analyze-token-summary" aria-label="Model token usage">
        {lanes.filter((lane) => lane.value !== null).map((lane) =>
          <span key={lane.label}>{lane.label} <strong>{lane.value}</strong>{lane.reported < lane.observed ? <small className="ml-1">({lane.reported}/{lane.observed} requests)</small> : null}</span>)}
        {lanes.some((lane) => lane.value === null)
          ? <span>{lanes.every((lane) => lane.value === null) ? "Token usage not reported" : "Token usage partly reported"}</span>
          : allExact ? <span>Provider reported</span> : null}
        {observedAttempts.length > 0 && (!allExact || lanes.some((lane) => lane.value === null)) ?
          <span>Reported subtotal · {reportedAttempts.length}/{observedAttempts.length} requests · partial or estimated</span> : null}
      </div>

      <section aria-labelledby={`${panelId}-graph-heading`} className="px-5 pb-4 pt-2">
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
            <span className="sr-only">
              {graphView === "graph"
                ? "Observed containment and dependencies · click a stage for recorded facts"
                : graphView === "tree"
                  ? "Explore stages and requests · aligned bars show overlap · ~ marks estimated live time"
                  : "Time runs left to right · overlap means parallel work · ~ marks estimated live time"}
            </span>
          </span>
          <span className="shrink-0 text-xs text-text-muted">
            {graph.nodes.length} stages {timelineOpen ? "· hide" : "· show"}
          </span>
        </button>

        <div className="explain-analyze-tree-toolbar">
        <div role="group" aria-label="Execution graph view" className="explain-analyze-view-switch">
          {(["tree", "timeline", "graph"] as const).map((view) => (
            <button key={view} type="button" aria-pressed={graphView === view}
              onClick={() => setGraphView(view)}>{view === "graph" ? "Graph" : view === "tree" ? "Tree" : "Timeline"}</button>
          ))}
        </div>
        {graphView === "tree" ? <>
          <input type="search" aria-label="Search execution stages" placeholder="Find a stage…" value={search} onChange={(event) => setSearch(event.target.value)} className="explain-analyze-search" />
          <button type="button" className="explain-analyze-text-action" onClick={() => setAllExpanded(true)}>Expand all</button>
          <button type="button" className="explain-analyze-text-action" onClick={() => setAllExpanded(false)}>Collapse all</button>
        </> : null}
        </div>
        {searchMatches ? <p role="status" className="py-2 text-xs text-text-muted">{searchMatches.matches} matching {searchMatches.matches === 1 ? "stage" : "stages"} · ancestors shown <button type="button" className="ml-2 text-accent" onClick={() => setSearch("")}>Clear search</button></p> : null}
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
                isLive={live && !closedClockDomains.has(clockDomainId)}
                missingEndNodeIds={missingEndNodeIds}
                visibleLimit={limit}
                expandedNodeIds={searchMatches ? searchMatches.included : expandedNodeIds}
                collapsedNodeIds={searchMatches ? new Set() : collapsedNodeIds}
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
        {graphView === "tree" ? <p className="explain-analyze-tree-help">↑ ↓ navigate · ← → collapse / expand · Enter inspect · ~ live estimates</p> : null}
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
  missingEndNodeIds,
  visibleLimit,
  expandedNodeIds,
  collapsedNodeIds,
  onToggleNode,
}: {
  graphView: GraphView;
  domainNumber: number;
  nodes: readonly ExplainAnalyzeNodeV1[];
  nowElapsedMs: number;
  isLive: boolean;
  missingEndNodeIds: ReadonlySet<string>;
  visibleLimit: number;
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
  const graphLayout = useMemo(
    () => graphView === "graph" ? layoutExplainAnalyzeGraph(nodes, { maxNodes: visibleLimit })[0] : undefined,
    [nodes, visibleLimit, graphView],
  );
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
            <div><dt className="text-text-muted">Outcome</dt><dd>{!selected.terminalObserved && (!isLive || missingEndNodeIds.has(selected.nodeId)) ? "End not recorded" : nodeStatus(selected).label}</dd></div>
          </dl>
          {selected.outcome === "failed" ? <p className="mt-3 text-xs text-danger">This stage failed. A failure reason was not included in these Explain facts.</p> : null}
          {nodes.some((node) => node.dependencyNodeIds.includes(selected.nodeId)) ? <div className="mt-3 text-xs text-text-muted">Dependent stages: {nodes.filter((node) => node.dependencyNodeIds.includes(selected.nodeId)).map((node) =>
            <button key={node.nodeId} type="button" className="ml-2 text-accent underline" onClick={() => inspect(node.nodeId)}>{node.label}</button>)}</div> : null}
          {selected.parentNodeId ? <p className="mt-3 text-xs text-text-muted">Parent: {nodeById.get(selected.parentNodeId)?.label ?? selected.parentNodeId}</p> : null}
          {selected.dependencyNodeIds.length > 0 ? <div className="mt-3 text-xs text-text-muted">Depends on: {selected.dependencyNodeIds.map((id) =>
            nodeById.has(id) ? <button key={id} type="button" className="ml-2 text-accent underline" onClick={() => inspect(id)}>{nodeById.get(id)?.label}</button>
              : <span key={id} className="ml-2">{id} (outside this timeline)</span>)}</div> : null}
          {selected.context ? explainAnalyzeContextSections(selected.context).map((section) => (
            <section key={section.title} className="mt-4 border-t border-border pt-3" aria-label={section.title}>
              <h5 className="text-xs font-semibold text-text">{section.title}</h5>
              <p className="mt-1 text-[10px] text-text-muted">{section.description}</p>
              <dl className="mt-2 grid gap-x-6 gap-y-1 sm:grid-cols-2">
                {section.rows.map((row) => <div key={row.label} className="flex justify-between gap-3 text-xs">
                  <dt className="text-text-muted">{row.label}</dt><dd className="tabular-nums text-text">{row.value}</dd>
                </div>)}
              </dl>
              {section.rows.length === 0 ? <p className="text-xs text-text-muted">No sections in this assembly.</p> : null}
            </section>
          )) : null}
          {selected.kind === "provider_attempt" ? <p className="mt-3 text-xs text-text-secondary">{requestIdentity(selected)} · {selected.usage ? formatUsageDetail(selected.usage) : "Token usage not reported"}</p> : null}
        </aside>
      ) : null;
  if (graphView === "graph") {
    return (
      <div className="explain-analyze-domain explain-analyze-graph-domain min-w-0 rounded-xl border border-border bg-bg/40 p-3">
        <div className="mb-3 flex justify-between text-[9px] font-medium uppercase tracking-wide text-text-muted">
          <span>Execution {domainNumber}</span>
          <span>{hasActiveNodes ? "~" : ""}{formatMs(domainEnd)} {hasActiveNodes ? "estimated live extent" : "recorded extent"}</span>
        </div>
        <GraphCanvas
          domain={graphLayout}
          nodes={nodes}
          nodeById={nodeById}
          selectedNodeId={selectedNodeId}
          onSelectNode={inspect}
          inspector={inspector}
          nowElapsedMs={nowElapsedMs}
          isLive={isLive}
          missingEndNodeIds={missingEndNodeIds}
        />
        {selected ? null : <p className="mt-3 text-xs text-text-muted">Select a stage to inspect its timing, outcome and reported usage.</p>}
      </div>
    );
  }
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
  missingEndNodeIds,
    nodeById,
    childrenByParent,
    visited,
    budget,
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
      <div className="overflow-x-auto pb-1" onKeyDown={graphView === "tree" ? navigateExplainTree : undefined}>
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
          </div> : null}
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

function GraphCanvas({
  domain,
  nodes,
  nodeById,
  selectedNodeId,
  onSelectNode,
  inspector,
  nowElapsedMs,
  isLive,
  missingEndNodeIds,
}: {
  domain: ExplainAnalyzeLayoutDomainV1 | undefined;
  nodes: readonly ExplainAnalyzeNodeV1[];
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>;
  selectedNodeId: string | null;
  onSelectNode: (nodeId: string) => void;
  inspector: ReactNode;
  nowElapsedMs: number;
  isLive: boolean;
  missingEndNodeIds: ReadonlySet<string>;
}) {
  const markerId = `explain-analyze-arrow-${useId().replace(/:/g, "")}`;
  const graphScrollRef = useRef<HTMLDivElement>(null);
  const didAutoFit = useRef(false);
  const [scale, setScale] = useState(1);
  const layoutNodes = domain?.nodes ?? [];
  const layoutNodeIds = new Set(layoutNodes.map((node) => node.nodeId));
  const layoutEdges = domain?.edges ?? [];
  const parentNodeIds = new Set(
    layoutEdges.filter((edge) => edge.kind === "parent").map((edge) => edge.sourceNodeId),
  );
  const selectedIsVisible = selectedNodeId !== null && layoutNodeIds.has(selectedNodeId);
  const domainWidth = domain?.width ?? 0;
  const domainHeight = domain?.height ?? 0;
  useEffect(() => {
    if (domainWidth <= 0 || didAutoFit.current) return;
    didAutoFit.current = true;
    const availableWidth = graphScrollRef.current?.clientWidth ?? 0;
    setScale(availableWidth > 0 ? clampGraphScale((availableWidth - 24) / domainWidth) : 1);
  }, [domainWidth, domainHeight]);
  const fitGraph = () => {
    if (!domain) return;
    const availableWidth = graphScrollRef.current?.clientWidth ?? 0;
    if (availableWidth <= 0) {
      setScale(0.78);
      return;
    }
    setScale(clampGraphScale((availableWidth - 24) / domain.width));
  };

  return (
    <div className="explain-analyze-graph-canvas-wrap">
      <div className="explain-analyze-graph-toolbar-row">
        <div aria-label="Graph legend" className="explain-analyze-graph-legend">
          <StatusLegend colorClass="bg-success" label="Completed" />
          <StatusLegend colorClass="bg-danger" label="Failed" />
          <StatusLegend colorClass="bg-accent" label="In progress" />
          <StatusLegend colorClass="bg-warning" label="Waiting" />
          <span className="explain-analyze-graph-key">
            <span className="explain-analyze-graph-key-line explain-analyze-graph-key-parent" aria-hidden="true" />
            Contains
          </span>
          <span className="explain-analyze-graph-key">
            <span className="explain-analyze-graph-key-line explain-analyze-graph-key-dependency" aria-hidden="true" />
            Runs after
          </span>
        </div>
        <div className="explain-analyze-graph-toolbar" role="group" aria-label="Graph zoom controls">
          <button type="button" aria-label="Zoom out graph" disabled={scale <= 0.55} onClick={() => setScale((current) => clampGraphScale(current - 0.1))}>−</button>
          <span aria-live="polite" className="tabular-nums">{Math.round(scale * 100)}%</span>
          <button type="button" aria-label="Zoom in graph" disabled={scale >= 1.5} onClick={() => setScale((current) => clampGraphScale(current + 0.1))}>+</button>
          <button type="button" aria-label="Fit graph" onClick={fitGraph}>Fit graph</button>
        </div>
      </div>
      <div ref={graphScrollRef} className="explain-analyze-graph-scroll" role="group" aria-label={`Execution graph ${domain?.clockDomainId ?? ""}`}>
        <div
          className="explain-analyze-graph-zoom-stage"
          style={{ width: (domain?.width ?? 0) * scale, height: (domain?.height ?? 0) * scale }}
        >
          <div
            className="explain-analyze-graph-canvas"
            style={{ width: domain?.width ?? 0, height: domain?.height ?? 0, transform: `scale(${scale})` }}
          >
          {domain ? (
            <svg
              className="explain-analyze-graph-edges"
              width={domain.width}
              height={domain.height}
              viewBox={`0 0 ${domain.width} ${domain.height}`}
              aria-hidden="true"
              focusable="false"
            >
              <defs>
                <marker id={`${markerId}-dependency`} viewBox="0 0 10 10" refX="9" refY="5" markerWidth="5" markerHeight="5" orient="auto-start-reverse">
                  <path d="M 0 0 L 10 5 L 0 10 z" className="explain-analyze-graph-arrow-dependency" />
                </marker>
              </defs>
              {layoutEdges.map((edge) => {
                const target = nodeById.get(edge.targetNodeId);
                const active = isLive && target !== undefined &&
                  !missingEndNodeIds.has(edge.targetNodeId) &&
                  !target.terminalObserved && target.kind !== "wait" && target.kind !== "admission";
                const selected = selectedNodeId !== null &&
                  (edge.sourceNodeId === selectedNodeId || edge.targetNodeId === selectedNodeId);
                return (
                  <path
                    key={`${edge.kind}:${edge.sourceNodeId}:${edge.targetNodeId}`}
                    d={edge.path}
                    className={cn(
                      "explain-analyze-graph-edge",
                      edge.kind === "dependency" && "explain-analyze-graph-edge-dependency",
                      active && "explain-analyze-graph-edge-active",
                      selected && "explain-analyze-graph-edge-selected",
                    )}
                    markerEnd={edge.kind === "dependency" ? `url(#${markerId}-dependency)` : undefined}
                  />
                );
              })}
            </svg>
          ) : null}
          {layoutNodes.map((position: ExplainAnalyzeLayoutNodeV1) => {
            const node = nodeById.get(position.nodeId);
            if (!node) return null;
            const nodeIsLive = isLive && !missingEndNodeIds.has(node.nodeId);
            const missingEnd = !node.terminalObserved && !nodeIsLive;
            const canEstimate = !node.terminalObserved && nodeIsLive;
            const status = missingEnd
              ? { label: "End not recorded", textClass: "text-warning", dotClass: "bg-warning" }
              : nodeStatus(node);
            const visualStatus = node.kind === "wait"
              ? { ...status, textClass: "text-warning", dotClass: "bg-warning" }
              : node.kind === "admission"
                ? { ...status, textClass: "text-text-muted", dotClass: "bg-text-muted" }
                : status.label === "Completed"
                  ? { ...status, textClass: "text-text-muted" }
                  : status;
            const endLabel = node.endElapsedMs !== undefined
              ? formatMs(node.endElapsedMs)
              : !canEstimate ? "Unknown" : `~${formatMs(nowElapsedMs)}`;
            const durationLabel = node.durationMs !== undefined
              ? formatMs(node.durationMs)
              : !canEstimate ? "Unknown" : `~${formatMs(Math.max(0, nowElapsedMs - node.startElapsedMs))}`;
            const usage = node.usage
              ? formatUsage(node.usage)
              : node.context
                ? formatExplainAnalyzeContext(node.context)
                : node.kind === "provider_attempt" ? "Token usage not reported" : "";
            const StageIcon = node.kind === "wait" || node.kind === "admission" ? Clock3
              : node.kind === "provider_attempt" ? Cpu
                : node.kind === "tool_call" ? Wrench
                  : node.kind === "turn" ? GitBranch : Layers;
            return (
              <button
                key={position.nodeId}
                type="button"
                data-node-id={position.nodeId}
                aria-label={`Inspect ${node.label}, ${durationLabel}, ${status.label}`}
                aria-pressed={selectedNodeId === node.nodeId}
                onClick={() => onSelectNode(node.nodeId)}
                className={cn(
                  "explain-analyze-graph-card",
                  `explain-analyze-graph-card-${node.kind}`,
                  `explain-analyze-graph-card-status-${visualStatus.textClass.replace(/^text-/, "")}`,
                  selectedNodeId === node.nodeId && "explain-analyze-graph-card-selected",
                  canEstimate && node.kind !== "wait" && node.kind !== "admission" && "explain-analyze-graph-card-active",
                  parentNodeIds.has(node.nodeId) && "explain-analyze-graph-card-parent",
                )}
                style={{ left: position.x, top: position.y, width: position.width, height: position.height }}
              >
                <span className="explain-analyze-graph-card-head">
                  <span className="explain-analyze-graph-card-icon"><StageIcon aria-hidden="true" /></span>
                  <strong className="explain-analyze-graph-card-title" title={node.label}>{node.label}</strong>
                </span>
                <span className="explain-analyze-graph-card-meta">
                  <b>{durationLabel}</b>
                  <span className={cn("explain-analyze-graph-card-status", visualStatus.textClass)}>
                    <span className={cn("explain-analyze-graph-card-dot", visualStatus.dotClass)} aria-hidden="true" />
                    {visualStatus.label}
                  </span>
                </span>
                <span className="explain-analyze-graph-card-interval">{formatMs(node.startElapsedMs)} → {endLabel}</span>
                {usage ? <span className="explain-analyze-graph-card-usage" title={node.usage ? formatUsageDetail(node.usage) : usage}>{usage}</span> : null}
              </button>
            );
          })}
          </div>
        </div>
      </div>
      {selectedNodeId && inspector ? (
        <div className="explain-analyze-graph-inspector">
          {!selectedIsVisible ? <p className="text-xs text-text-muted">Selected stage is outside the visible graph window. Its recorded details are shown here.</p> : null}
          {inspector}
        </div>
      ) : null}
      {nodes.length > layoutNodes.length ? (
        <p className="mt-2 text-[10px] text-text-muted">Showing {layoutNodes.length} of {nodes.length} stages in this graph window.</p>
      ) : null}
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
  missingEndNodeIds,
  nodeById,
  childrenByParent,
  visited,
  budget,
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
  missingEndNodeIds: ReadonlySet<string>;
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>;
  childrenByParent: ReadonlyMap<string, readonly ExplainAnalyzeNodeV1[]>;
  visited: Set<string>;
  budget: TimelineBudget;
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
    !collapsedNodeIds.has(node.nodeId)
  );
  const nodeIsLive = isLive && !missingEndNodeIds.has(node.nodeId);
  const missingEnd = !node.terminalObserved && !nodeIsLive;
  const canEstimate = !node.terminalObserved && nodeIsLive;
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
  const usage = node.usage ? formatUsage(node.usage) : node.context ? formatExplainAnalyzeContext(node.context) : "";
  const treeDepth = Math.min(depth, 8);
  const indent = treeDepth * (graphView === "tree" ? 20 : 12);

  return (
    <div key={node.nodeId} className="explain-analyze-tree-node">
      <div data-tree-node-id={node.nodeId} data-tree-parent-id={node.parentNodeId} className={cn(
        "explain-analyze-lane relative items-center border-b border-border/60",
        graphView === "tree" ? "explain-analyze-tree-columns" : "explain-analyze-grid",
        graphView === "tree" && canEstimate && node.kind !== "wait" && node.kind !== "admission" && "explain-analyze-tree-active",
        selectedNodeId === node.nodeId && "explain-analyze-lane-selected",
        graphView === "timeline" && cursorMs !== null && node.startElapsedMs > cursorMs && "explain-analyze-lane-future",
        children.length > 0 && "explain-analyze-group-row",
        node.outcome === "failed" && "explain-analyze-tree-failed",
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
              {graphView === "tree" ? <button type="button" className={cn("explain-analyze-stage-title", node.kind === "wait" && "text-warning", node.kind === "admission" && "text-text-muted")}
                aria-label={`Inspect ${node.label}, ${durationLabel}, ${status.label}`}
                aria-pressed={selectedNodeId === node.nodeId} onClick={() => onSelectNode(node.nodeId)}>
                {node.label}
              </button> : <span className="truncate text-[11px] font-semibold text-text" title={node.label}>{node.label}</span>}
              {request ? (
                <span className="shrink-0 rounded border border-border bg-bg px-1 py-px text-[8px] font-medium text-text-muted">
                  {request}
                </span>
              ) : null}
              {graphView === "timeline" && children.length > 0 ? (
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
            {graphView === "tree" ? (
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
            </> : node.context ? usage : node.kind === "provider_attempt" ? "Unreported" : ""}
          </span>
            ) : null}
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
          aria-label={`Inspect ${node.label}${children.length > 0 ? `, parent stage with ${children.length} nested ${children.length === 1 ? "stage" : "stages"}` : ", work stage"}, ${formatMs(node.startElapsedMs)} to ${endLabel}, ${durationLabel}${!node.terminalObserved && nodeIsLive ? " estimated elapsed so far" : ""}, ${status.label}`}
            aria-pressed={selectedNodeId === node.nodeId}
            onClick={() => onSelectNode(node.nodeId)}
            className={cn(
              "explain-analyze-bar absolute top-1 h-6 min-w-[4px] rounded-md",
              status.barClass,
              children.length > 0 && "explain-analyze-parent-bar",
              children.length === 0 && !node.terminalObserved && nodeIsLive && "explain-analyze-bar-active",
            )}
            style={{ left: `${left}%`, width: `${width}%` }}
           />
        </div>
        <span className="text-right text-[9px] tabular-nums text-text-muted">
          {formatMs(node.startElapsedMs)}–{endLabel}
        </span></> : null}
        <span className="explain-analyze-duration text-right text-[10px] font-semibold tabular-nums text-text-secondary">{durationLabel}</span>
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
  missingEndNodeIds,
            nodeById,
            childrenByParent,
            visited,
            budget,
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
    const reported = values.filter((lane) => lane !== undefined).length;
    const value = reported > 0
      ? Number.isSafeInteger(total) ? total.toLocaleString() : "Exceeds precise range"
      : null;
    return { label, value, reported, observed: nodes.length };
  });
}

function requestIdentity(node: ExplainAnalyzeNodeV1) {
  const round = node.roundIndex === undefined ? null : node.roundIndex + 1;
  const attempt = node.attemptIndex === undefined ? null : node.attemptIndex + 1;
  if (round === null && attempt === null) return "Model request";
  if (round !== null && attempt !== null) return `Round ${round} · request ${attempt}`;
  return round !== null ? `Round ${round}` : `Request ${attempt}`;
}

function clampGraphScale(value: number) {
  return Math.min(1.5, Math.max(0.55, Math.round(value * 100) / 100));
}

function nodeStatus(node: ExplainAnalyzeNodeV1) {
  if (node.conflicted) {
    return { label: "Conflicting facts", textClass: "text-warning", dotClass: "bg-warning", barClass: "bg-warning" };
  }
  if (!node.terminalObserved) {
    if (node.kind === "wait") {
      return { label: "Waiting", textClass: "text-warning", dotClass: "bg-warning", barClass: "bg-warning" };
    }
    if (node.kind === "admission") {
      return { label: "Awaiting dispatch", textClass: "text-text-muted", dotClass: "bg-text-muted", barClass: "bg-text-muted" };
    }
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

function navigateExplainTree(event: KeyboardEvent<HTMLDivElement>) {
  const target = event.target as HTMLElement;
  if (!target.matches(".explain-analyze-stage-title")) return;
  const stages = [...event.currentTarget.querySelectorAll<HTMLButtonElement>(".explain-analyze-stage-title")];
  const index = stages.indexOf(target as HTMLButtonElement);
  const row = target.closest<HTMLElement>("[data-tree-node-id]");
  if (index < 0 || !row) return;
  const toggle = row.querySelector<HTMLButtonElement>("button[aria-expanded]");
  let next: HTMLButtonElement | undefined;
  switch (event.key) {
    case "ArrowDown": next = stages[Math.min(index + 1, stages.length - 1)]; break;
    case "ArrowUp": next = stages[Math.max(index - 1, 0)]; break;
    case "Home": next = stages[0]; break;
    case "End": next = stages[stages.length - 1]; break;
    case "ArrowRight":
      if (toggle?.getAttribute("aria-expanded") === "false") toggle.click();
      else next = stages[index + 1];
      break;
    case "ArrowLeft":
      if (toggle?.getAttribute("aria-expanded") === "true") toggle.click();
      else next = stages.find((stage) => stage.closest<HTMLElement>("[data-tree-node-id]")?.dataset.treeNodeId === row.dataset.treeParentId);
      break;
    default: return;
  }
  event.preventDefault();
  next?.focus({ preventScroll: true });
  next?.scrollIntoView?.({ block: "nearest" });
}

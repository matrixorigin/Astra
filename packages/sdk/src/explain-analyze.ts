import type {
  ExplainAnalyzeEventV1,
  ExplainAnalyzeUsageV1,
} from "./types";

export type ExplainAnalyzeNodeV1 = {
  nodeId: string;
  runId: string;
  turnId: string;
  clockDomainId: string;
  kind: ExplainAnalyzeEventV1["kind"];
  label: string;
  parentNodeId?: string;
  dependencyNodeIds: string[];
  roundIndex?: number;
  attemptIndex?: number;
  startElapsedMs: number;
  endElapsedMs?: number;
  durationMs?: number;
  outcome?: ExplainAnalyzeEventV1["outcome"];
  usage?: ExplainAnalyzeUsageV1;
  startObserved: boolean;
  terminalObserved: boolean;
  conflicted: boolean;
};

export type ExplainAnalyzeDiagnosticV1 = {
  code:
    | "invalid_event"
    | "conflicting_fact"
    | "missing_parent"
    | "missing_dependency"
    | "parent_cycle"
    | "dependency_cycle"
    | "unresolved_terminal_node";
  nodeId?: string;
  relatedNodeId?: string;
};

export type ExplainAnalyzeGraphV1 = {
  /** Internal consistency of observed facts, not a claim of instrumentation coverage. */
  integrity: "consistent" | "unknown";
  diagnostics: ExplainAnalyzeDiagnosticV1[];
  nodes: ExplainAnalyzeNodeV1[];
  duplicateEventCount: number;
  conflictedNodeIds: string[];
};

const nodeKinds = new Set([
  "run",
  "turn",
  "admission",
  "preparation",
  "context_assembly",
  "model_round",
  "provider_attempt",
  "tool_batch",
  "tool_call",
  "wait",
  "child_run",
  "settlement",
]);
const outcomes = new Set([
  "completed",
  "succeeded",
  "failed",
  "cancelled",
  "interrupted",
  "blocked",
  "waiting",
  "rejected",
  "reused",
  "suppressed",
  "deferred",
  "resolved",
  "fallback",
  "unavailable",
  "delegated",
]);
const allowedEventKeys = new Set([
  "type",
  "index",
  "schema_version",
  "event_id",
  "run_id",
  "turn_id",
  "node_id",
  "parent_node_id",
  "dependency_node_ids",
  "producer_id",
  "clock_domain_id",
  "kind",
  "round_index",
  "attempt_index",
  "label",
  "transition",
  "elapsed_ms",
  "start_elapsed_ms",
  "duration_ms",
  "outcome",
  "usage",
]);

/** Runtime validation for events received over SSE or restored from storage. */
export function isExplainAnalyzeEventV1(
  value: unknown,
): value is ExplainAnalyzeEventV1 {
  if (!isRecord(value) || Object.keys(value).some((key) => !allowedEventKeys.has(key))) {
    return false;
  }
  if (
    value.type !== "explain_analyze" ||
    value.schema_version !== 1 ||
    !nonEmptyString(value.event_id, 512) ||
    !nonEmptyString(value.run_id, 512) ||
    !nonEmptyString(value.turn_id, 512) ||
    !nonEmptyString(value.node_id, 512) ||
    !nonEmptyString(value.producer_id, 512) ||
    !nonEmptyString(value.clock_domain_id, 512) ||
    !nonEmptyString(value.label, 160) ||
    !nodeKinds.has(String(value.kind)) ||
    !isNonNegativeInteger(value.elapsed_ms) ||
    (value.round_index !== undefined && !isNonNegativeInteger(value.round_index)) ||
    (value.attempt_index !== undefined && !isNonNegativeInteger(value.attempt_index)) ||
    (value.parent_node_id !== undefined && !nonEmptyString(value.parent_node_id, 512)) ||
    (value.dependency_node_ids !== undefined &&
      (!Array.isArray(value.dependency_node_ids) ||
        !value.dependency_node_ids.every((id) => nonEmptyString(id, 512))))
  ) {
    return false;
  }
  if (
    (value.kind === "model_round" && value.round_index === undefined) ||
    (value.kind === "provider_attempt" &&
      (value.round_index === undefined || value.attempt_index === undefined))
  ) {
    return false;
  }
  if (value.transition === "started") {
    return (
      value.start_elapsed_ms === undefined &&
      value.duration_ms === undefined &&
      value.outcome === undefined &&
      value.usage === undefined
    );
  }
  if (
    value.transition !== "finished" ||
    !isNonNegativeInteger(value.start_elapsed_ms) ||
    !isNonNegativeInteger(value.duration_ms) ||
    !outcomes.has(String(value.outcome)) ||
    value.start_elapsed_ms > value.elapsed_ms ||
    Math.abs(value.elapsed_ms - value.start_elapsed_ms - value.duration_ms) > 1
  ) {
    return false;
  }
  return value.usage === undefined || isExplainAnalyzeUsage(value.usage);
}

function isExplainAnalyzeUsage(value: unknown): value is ExplainAnalyzeUsageV1 {
  if (!isRecord(value)) return false;
  const validBasis = ["provider_exact", "provider_partial", "runtime_estimated"].includes(
    String(value.basis),
  );
  const knownLanes = [
    "fresh_input_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "output_tokens",
  ];
  return (
    validBasis &&
    Object.keys(value).every((key) => key === "basis" || knownLanes.includes(key)) &&
    knownLanes.some((key) => isNonNegativeInteger(value[key])) &&
    knownLanes.every((key) => value[key] === undefined || isNonNegativeInteger(value[key]))
  );
}

/** Idempotently rebuild the graph; terminal facts can reconstruct a missed start. */
export function reduceExplainAnalyzeEvents(
  events: readonly unknown[],
): ExplainAnalyzeGraphV1 {
  const seenEvents = new Map<string, string>();
  const nodes = new Map<string, ExplainAnalyzeNodeV1>();
  const conflictedNodeIds = new Set<string>();
  let duplicateEventCount = 0;
  const diagnostics: ExplainAnalyzeDiagnosticV1[] = [];

  for (const value of events) {
    if (!isExplainAnalyzeEventV1(value)) {
      if (isRecord(value) && value.type === "explain_analyze") {
        diagnostics.push({ code: "invalid_event" });
      }
      continue;
    }
    const fingerprint = explainAnalyzeFactFingerprint(value);
    if (fingerprint === null) continue;
    const seen = seenEvents.get(value.event_id);
    if (seen !== undefined) {
      duplicateEventCount += 1;
      if (seen !== fingerprint) {
        conflictedNodeIds.add(value.node_id);
        // A reused event identity can also point at a different node.
        const original = JSON.parse(seen) as { node_id: string };
        conflictedNodeIds.add(original.node_id);
      }
      continue;
    }
    seenEvents.set(value.event_id, fingerprint);

    let node = nodes.get(value.node_id);
    if (!node) {
      node = {
        nodeId: value.node_id,
        runId: value.run_id,
        turnId: value.turn_id,
        clockDomainId: value.clock_domain_id,
        kind: value.kind,
        label: value.label,
        ...(value.parent_node_id ? { parentNodeId: value.parent_node_id } : {}),
        dependencyNodeIds: value.dependency_node_ids ?? [],
        ...(value.round_index !== undefined ? { roundIndex: value.round_index } : {}),
        ...(value.attempt_index !== undefined ? { attemptIndex: value.attempt_index } : {}),
        startElapsedMs:
          value.transition === "started"
            ? value.elapsed_ms
            : (value.start_elapsed_ms ?? value.elapsed_ms),
        ...(value.transition === "finished"
          ? {
              endElapsedMs: value.elapsed_ms,
              durationMs: value.duration_ms,
              outcome: value.outcome,
              ...(value.usage ? { usage: value.usage } : {}),
            }
          : {}),
        startObserved: value.transition === "started",
        terminalObserved: value.transition === "finished",
        conflicted: false,
      };
      nodes.set(value.node_id, node);
      continue;
    }

    if (
      node.runId !== value.run_id ||
      node.turnId !== value.turn_id ||
      node.clockDomainId !== value.clock_domain_id ||
      node.kind !== value.kind ||
      node.label !== value.label ||
      node.parentNodeId !== value.parent_node_id ||
      node.roundIndex !== value.round_index ||
      node.attemptIndex !== value.attempt_index ||
      !sameStrings(node.dependencyNodeIds, value.dependency_node_ids ?? [])
    ) {
      node.conflicted = true;
    }

    if (value.transition === "started") {
      if (
        (node.startObserved || node.terminalObserved) &&
        node.startElapsedMs !== value.elapsed_ms
      ) {
        node.conflicted = true;
      } else if (!node.startObserved) {
        node.startElapsedMs = value.elapsed_ms;
        node.startObserved = true;
      }
    } else if (node.terminalObserved) {
      if (
        node.startElapsedMs !== value.start_elapsed_ms ||
        node.endElapsedMs !== value.elapsed_ms ||
        node.durationMs !== value.duration_ms ||
        node.outcome !== value.outcome ||
        stableJson(node.usage ?? null) !== stableJson(value.usage ?? null)
      ) {
        node.conflicted = true;
      }
    } else {
      if (node.startObserved && node.startElapsedMs !== value.start_elapsed_ms) {
        node.conflicted = true;
      } else if (!node.startObserved && value.start_elapsed_ms !== undefined) {
        node.startElapsedMs = value.start_elapsed_ms;
      }
      node.endElapsedMs = value.elapsed_ms;
      node.durationMs = value.duration_ms;
      node.outcome = value.outcome;
      node.usage = value.usage;
      node.terminalObserved = true;
    }
    if (node.conflicted) conflictedNodeIds.add(node.nodeId);
  }

  const orderedNodes = [...nodes.values()];
  const nodeById = new Map(orderedNodes.map((node) => [node.nodeId, node]));
  const terminalTurns = new Set(orderedNodes
    .filter((node) => node.kind === "turn" && node.terminalObserved)
    .map((node) => JSON.stringify([node.runId, node.turnId, node.clockDomainId])));
  for (const node of orderedNodes) {
    if (conflictedNodeIds.has(node.nodeId)) node.conflicted = true;
    if (node.parentNodeId && !nodeById.has(node.parentNodeId)) {
      diagnostics.push({ code: "missing_parent", nodeId: node.nodeId, relatedNodeId: node.parentNodeId });
    }
    for (const dependency of node.dependencyNodeIds) {
      if (!nodeById.has(dependency)) {
        diagnostics.push({ code: "missing_dependency", nodeId: node.nodeId, relatedNodeId: dependency });
      }
    }
    if (!node.terminalObserved && terminalTurns.has(JSON.stringify([node.runId, node.turnId, node.clockDomainId]))) {
      diagnostics.push({ code: "unresolved_terminal_node", nodeId: node.nodeId });
    }
  }
  for (const nodeId of conflictedNodeIds) diagnostics.push({ code: "conflicting_fact", nodeId });
  diagnoseCycles(nodeById, "parent_cycle", (node) => node.parentNodeId ? [node.parentNodeId] : [], diagnostics);
  diagnoseCycles(nodeById, "dependency_cycle", (node) => node.dependencyNodeIds, diagnostics);
  diagnostics.sort((left, right) =>
    left.code.localeCompare(right.code) ||
    (left.nodeId ?? "").localeCompare(right.nodeId ?? "") ||
    (left.relatedNodeId ?? "").localeCompare(right.relatedNodeId ?? ""));
  orderedNodes.sort((left, right) => {
    const timelineOrder =
      left.clockDomainId.localeCompare(right.clockDomainId) ||
      left.startElapsedMs - right.startElapsedMs;
    if (timelineOrder !== 0) return timelineOrder;
    return nodeDepth(left, nodeById) - nodeDepth(right, nodeById) ||
      left.nodeId.localeCompare(right.nodeId);
  });
  return {
    integrity: diagnostics.length === 0 ? "consistent" : "unknown",
    diagnostics,
    nodes: orderedNodes,
    duplicateEventCount,
    conflictedNodeIds: [...conflictedNodeIds].sort(),
  };
}

/** Iterative DFS avoids overflowing the JS stack on long histories. */
function diagnoseCycles(
  nodes: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  code: "parent_cycle" | "dependency_cycle",
  edges: (node: ExplainAnalyzeNodeV1) => readonly string[],
  diagnostics: ExplainAnalyzeDiagnosticV1[],
) {
  const colors = new Map<string, "visiting" | "done">();
  for (const root of [...nodes.keys()].sort()) {
    if (colors.has(root)) continue;
    const stack = [{ id: root, edges: edges(nodes.get(root)!), next: 0 }];
    colors.set(root, "visiting");
    while (stack.length > 0) {
      const frame = stack[stack.length - 1];
      if (frame.next === frame.edges.length) {
        colors.set(frame.id, "done");
        stack.pop();
        continue;
      }
      const target = frame.edges[frame.next++];
      const targetNode = nodes.get(target);
      if (!targetNode) continue;
      if (colors.get(target) === "visiting") {
        diagnostics.push({ code, nodeId: frame.id, relatedNodeId: target });
      } else if (!colors.has(target)) {
        colors.set(target, "visiting");
        stack.push({ id: target, edges: edges(targetNode), next: 0 });
      }
    }
  }
}

/** Compare a runtime fact independently of the cursor attached by a stream. */
export function explainAnalyzeFactFingerprint(value: unknown): string | null {
  if (!isExplainAnalyzeEventV1(value)) return null;
  // The durable stream cursor belongs to transport, not to event identity.
  // A replay adds `index`; the same fact must remain idempotent.
  const canonicalFact: Record<string, unknown> = { ...value };
  delete canonicalFact.index;
  return stableJson(canonicalFact);
}

export function explainAnalyzeMaxConcurrency(
  graph: ExplainAnalyzeGraphV1,
): number | null {
  if (graph.integrity === "unknown" || graph.nodes.some((node) => !node.terminalObserved)) return null;
  const parentIds = new Set(
    graph.nodes.flatMap((node) =>
      node.parentNodeId ? [`${node.clockDomainId}\u0000${node.parentNodeId}`] : [],
    ),
  );
  const leaves = graph.nodes.filter(
    (node) => !parentIds.has(`${node.clockDomainId}\u0000${node.nodeId}`),
  );
  const byClock = new Map<string, Array<{ at: number; delta: number }>>();
  for (const node of leaves) {
    const end =
      node.endElapsedMs ??
      (node.durationMs !== undefined ? node.startElapsedMs + node.durationMs : undefined);
    if (end === undefined || end <= node.startElapsedMs) continue;
    const points = byClock.get(node.clockDomainId) ?? [];
    points.push({ at: node.startElapsedMs, delta: 1 }, { at: end, delta: -1 });
    byClock.set(node.clockDomainId, points);
  }
  if (byClock.size === 0) return null;
  let maximum = 0;
  for (const points of byClock.values()) {
    points.sort((left, right) => left.at - right.at || left.delta - right.delta);
    let active = 0;
    for (const point of points) {
      active += point.delta;
      maximum = Math.max(maximum, active);
    }
  }
  return maximum;
}

/** Produce a self-contained, script-free HTML view of the same graph facts. */
export function renderExplainAnalyzeHtml(
  events: readonly unknown[],
  options: { degraded?: boolean; title?: string } = {},
): string {
  const graph = reduceExplainAnalyzeEvents(events);
  const title = escapeHtml(options.title?.trim() || "Explain Analyze");
  const byClock = new Map<string, ExplainAnalyzeNodeV1[]>();
  const nodeById = new Map(graph.nodes.map((node) => [node.nodeId, node]));
  for (const node of graph.nodes) {
    const group = byClock.get(node.clockDomainId) ?? [];
    group.push(node);
    byClock.set(node.clockDomainId, group);
  }
  const domains = [...byClock.entries()]
    .map(([, nodes], index) => renderHtmlTimelineDomain(nodes, index, byClock.size, nodeById))
    .join("");
  const providerAttempts = graph.nodes.filter(
    (node) => node.kind === "provider_attempt" && node.terminalObserved,
  );
  const turnDurations = graph.nodes
    .filter((node) => node.kind === "turn" && node.terminalObserved)
    .map((node) => node.durationMs)
    .filter((duration): duration is number => duration !== undefined);
  const turnDuration = turnDurations.length > 0 ? Math.max(...turnDurations) : undefined;
  const slowestRequest = [...providerAttempts]
    .filter((node) => node.durationMs !== undefined)
    .sort((left, right) => (right.durationMs ?? 0) - (left.durationMs ?? 0))[0];
  const failedAttempts = providerAttempts.filter(
    (node) => node.outcome === "failed" || node.outcome === "interrupted",
  ).length;
  const tokenTotals = summarizeUsageLanes(providerAttempts);
  const activeTurn = graph.nodes.some((node) => node.kind === "turn" && !node.terminalObserved);
  const openNodes = graph.nodes.filter((node) => !node.terminalObserved);
  const terminalTurn = graph.nodes.some((node) => node.kind === "turn" && node.terminalObserved);
  const hasUnresolvedTerminalNodes = terminalTurn && openNodes.length > 0;
  const hasConflict = graph.conflictedNodeIds.length > 0;
  const isDegraded = options.degraded || graph.integrity === "unknown" || hasConflict || hasUnresolvedTerminalNodes;
  const warning =
    isDegraded
      ? `<aside class="warning" role="status"><span class="warning-icon" aria-hidden="true">!</span><span><strong>Some execution facts are missing or conflict.</strong><br>Refresh this run's saved event history to repair the graph.</span></aside>`
      : "";
  const concurrency = isDegraded ? null : explainAnalyzeMaxConcurrency(graph);
  const status = turnDurations.length > 0
    ? (graph.nodes.find((node) => node.kind === "turn" && node.terminalObserved)?.outcome ?? "completed")
    : activeTurn ? "waiting" : "unavailable";
  const statusLabel = isDegraded ? "Incomplete" : humanOutcome(status);
  const metrics = [
    ["Turn time", turnDuration === undefined ? (activeTurn ? "In progress" : "Not recorded") : formatMs(turnDuration), "From first turn event to its terminal event"],
    ["Slowest model request", slowestRequest?.durationMs === undefined ? "Not recorded" : formatMs(slowestRequest.durationMs), slowestRequest ? `${requestIdentity(slowestRequest)} · ${humanOutcome(slowestRequest.outcome ?? "unavailable")}` : "Appears when a request completes"],
    ["Provider requests", String(providerAttempts.length), failedAttempts > 0 ? `${failedAttempts} failed or interrupted` : "Physical requests, including retries"],
    ["Peak parallel work", concurrency === null ? "Not recorded" : `${concurrency} at once`, "Measured within each worker timeline"],
  ];
  const metricCards = metrics.map(([label, value, note], index) =>
    `<article class="metric${index === 0 ? " metric-primary" : ""}"><span class="metric-label">${label}</span><strong>${escapeHtml(value)}</strong><small>${escapeHtml(note)}</small></article>`,
  ).join("");
  const tokenCards = tokenTotals.map(({ label, value }) =>
    `<article class="token-metric"><span>${label}</span><strong>${value ?? "Not fully reported"}</strong><small>tokens</small></article>`,
  ).join("");
  const statusClass = isDegraded ? "state-running" : status === "failed" || status === "interrupted" ? "state-failed" : status === "waiting" ? "state-running" : "state-complete";
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light"><title>${title} · Explain Analyze</title><style>
  :root{color-scheme:light;--ink:#16233b;--muted:#71809a;--line:#e6ebf2;--panel:#fff;--canvas:#f4f7fb;--blue:#5278e7;--blue-soft:#edf2ff;--green:#16836d;--green-soft:#e8f7f2;--red:#c6495a;--red-soft:#fff0f1;--amber:#a36b13;--amber-soft:#fff7e6;font:14px/1.5 Inter,ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}*{box-sizing:border-box}body{margin:0;background:radial-gradient(ellipse at 12% 0%,#e7edff 0,transparent 30%),var(--canvas);color:var(--ink)}.shell{max-width:1280px;margin:0 auto;padding:36px 32px 56px}.topline{display:flex;align-items:center;justify-content:space-between;margin-bottom:38px}.brand{display:flex;align-items:center;gap:10px;color:#344463;font-size:12px;font-weight:750;letter-spacing:.11em;text-transform:uppercase}.brand-mark{display:grid;width:30px;height:30px;place-items:center;border-radius:9px;background:linear-gradient(145deg,#668af5,#4663d2);color:#fff;font-size:16px;box-shadow:0 5px 14px #4663d233}.snapshot{border:1px solid #dfe5ee;border-radius:999px;background:#ffffffa8;color:var(--muted);padding:6px 11px;font-size:11px}.report-head{display:flex;align-items:flex-end;justify-content:space-between;gap:20px;margin-bottom:24px}.eyebrow{margin:0 0 8px;color:var(--blue);font-size:11px;font-weight:800;letter-spacing:.13em;text-transform:uppercase}.report-head h1{margin:0;font-size:clamp(25px,3vw,36px);line-height:1.15;letter-spacing:-.04em}.subtitle{margin:9px 0 0;color:var(--muted);font-size:14px}.state{display:inline-flex;align-items:center;gap:8px;padding:8px 12px;border:1px solid;border-radius:999px;font-size:12px;font-weight:700;white-space:nowrap}.state:before{content:"";width:7px;height:7px;border-radius:50%;background:currentColor}.state-complete{border-color:#cbe9de;background:var(--green-soft);color:var(--green)}.state-failed{border-color:#f3cfd3;background:var(--red-soft);color:var(--red)}.state-running{border-color:#d9e2ff;background:var(--blue-soft);color:var(--blue)}.metrics{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:12px;margin-bottom:16px}.metric{min-width:0;padding:17px 18px 15px;border:1px solid #e5eaf2;border-radius:15px;background:linear-gradient(155deg,#fff,#fbfcff);box-shadow:0 4px 14px #23345108}.metric-primary{border-color:#d9e2ff;background:linear-gradient(145deg,#f5f7ff,#fff)}.metric-label{display:block;margin-bottom:10px;color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.04em}.metric strong{display:block;overflow:hidden;color:var(--ink);font-size:24px;line-height:1.1;letter-spacing:-.04em;text-overflow:ellipsis;white-space:nowrap}.metric small{display:block;overflow:hidden;margin-top:8px;color:var(--muted);font-size:11px;text-overflow:ellipsis;white-space:nowrap}.panel{border:1px solid #e2e8f0;border-radius:17px;background:var(--panel);box-shadow:0 8px 25px #21345109}.token-panel{margin-bottom:16px;padding:19px 21px 17px}.panel-head{display:flex;align-items:flex-start;justify-content:space-between;gap:16px}.panel-head h2{margin:0;font-size:15px;letter-spacing:-.015em}.panel-head p{margin:4px 0 0;color:var(--muted);font-size:11px}.source-tag{flex:0 0 auto;border:1px solid #dce5f6;border-radius:999px;background:#f7f9ff;color:#526a9e;padding:5px 9px;font-size:10px;font-weight:700}.token-grid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:0;margin-top:16px}.token-metric{padding:2px 16px;border-left:1px solid var(--line)}.token-metric:first-child{padding-left:0;border-left:0}.token-metric span{display:block;color:var(--muted);font-size:11px}.token-metric strong{display:block;margin-top:5px;font-size:20px;letter-spacing:-.035em}.token-metric small{color:var(--muted);font-size:10px}.graph-panel{overflow:hidden}.graph-head{padding:20px 22px 16px;border-bottom:1px solid var(--line)}.graph-head h2{margin:0;font-size:16px;letter-spacing:-.02em}.graph-head p{margin:5px 0 0;color:var(--muted);font-size:11px}.legend{display:flex;flex-wrap:wrap;gap:12px;margin-top:13px}.legend-item{display:inline-flex;align-items:center;gap:6px;color:#697893;font-size:10px}.legend-dot{width:8px;height:8px;border-radius:50%;background:#9aabc8}.legend-dot.complete{background:#1a9a7e}.legend-dot.failed{background:#d95364}.legend-dot.running{background:#5278e7;box-shadow:0 0 0 3px #5278e71a}.legend-dot.waiting{background:#d19733}.bar-key{display:inline-block;width:18px;height:8px;border-radius:3px}.bar-key.group{border:1px dashed #8492a8;background:#e8edf5}.bar-key.work{background:#16836d}.warning{display:flex;align-items:flex-start;gap:10px;margin:16px 22px 0;padding:12px 14px;border:1px solid #f0ddb4;border-radius:11px;background:var(--amber-soft);color:#785311;font-size:12px}.warning-icon{display:grid;flex:0 0 auto;width:18px;height:18px;place-items:center;border-radius:50%;background:#f3dfb4;font-weight:800}.graph-scroll{overflow-x:auto;padding:4px 20px 22px}.timeline{min-width:850px}.axis-row,.graph-row{display:grid;grid-template-columns:minmax(235px,290px) minmax(290px,1fr) 112px 66px;gap:14px;align-items:center}.axis-row{height:42px;border-bottom:1px solid var(--line)}.axis-track{position:relative;height:100%;border-bottom:1px solid #dfe5ed}.axis-tick{position:absolute;bottom:4px;transform:translateX(-50%);color:#8090a9;font-size:10px;font-variant-numeric:tabular-nums;white-space:nowrap}.axis-tick:first-child{transform:none}.axis-tick:last-child{transform:translateX(-100%)}.axis-tick:after{position:absolute;top:19px;left:50%;width:1px;height:8px;background:#dfe5ed;content:""}.axis-tick:first-child:after{left:0}.axis-tick:last-child:after{left:100%}.axis-side{color:#93a0b4;font-size:9px;font-weight:700;letter-spacing:.08em;text-transform:uppercase}.domain{margin-top:12px}.domain:first-of-type{margin-top:0}.domain-label{margin:0 0 1px;padding:12px 0 4px;color:#586987;font-size:10px;font-weight:800;letter-spacing:.09em;text-transform:uppercase}.graph-tree{position:relative}.node-group{display:block}.node-group>summary{display:grid;width:100%;list-style:none;cursor:pointer}.node-group>summary::-webkit-details-marker{display:none}.node-group>summary::marker{content:""}.node-group>summary:focus-visible{outline:2px solid #7897f3;outline-offset:2px;border-radius:7px}.node-group:not([open])>.node-children{display:none}.graph-row{display:grid;width:100%;min-height:62px;padding:9px 0;border-bottom:1px solid #f0f2f6;transition:background .15s}.graph-row:hover{background:#fafbfe}.graph-row.is-group{background:linear-gradient(90deg,#f7f8ff 0,transparent 70%)}.node-copy{min-width:0;padding-left:calc(var(--depth,0)*14px);position:relative}.node-copy.nested:before{position:absolute;top:8px;left:calc(var(--indent,0px) - 7px);width:12px;border-top:1px solid #d8e0ea;content:"";pointer-events:none}.node-titleline{display:flex;min-width:0;align-items:center;gap:7px}.node-title{overflow:hidden;color:#263650;font-size:12px;font-weight:700;text-overflow:ellipsis;white-space:nowrap}.request-chip{flex:0 0 auto;border:1px solid #e4e9f1;border-radius:6px;background:#f8f9fc;color:#64738e;padding:2px 5px;font-size:9px;font-weight:700}.toggle{display:inline-grid;flex:0 0 auto;width:16px;height:16px;place-items:center;border:1px solid #e2e7ef;border-radius:5px;background:#fff;color:#74839c;font-size:11px;transition:transform .15s}.node-group[open]>summary .toggle{transform:rotate(90deg)}.node-leaf-mark{display:inline-block;flex:0 0 auto;width:7px;height:7px;margin:0 4.5px;border-radius:2px;background:#b5c0d1}.node-meta{display:flex;min-width:0;align-items:center;gap:7px;overflow:hidden;margin-top:5px;color:#77859b;font-size:10px;text-overflow:ellipsis;white-space:nowrap}.node-status{display:inline-flex;flex:0 0 auto;align-items:center;gap:4px;color:#73819a}.node-status:before{width:6px;height:6px;border-radius:50%;background:#9aabc8;content:""}.status-complete .node-status:before{background:#1a9a7e}.status-failed .node-status{color:#b54555}.status-failed .node-status:before{background:#d95364}.status-running .node-status:before{background:#5278e7;box-shadow:0 0 0 3px #5278e71a}.status-waiting .node-status:before{background:#d19733}.status-conflicted .node-status{color:#9a6817}.status-conflicted .node-status:before{background:#d19733}.node-usage{overflow:hidden;color:#61718e;text-overflow:ellipsis}.node-dependencies{overflow:hidden;margin-top:3px;color:#72819a;font-size:9px;text-overflow:ellipsis;white-space:nowrap}.node-children{position:relative;display:block}.node-children:before{position:absolute;top:0;bottom:30px;left:calc(var(--child-indent,0px) + 7px);border-left:1px solid #d8e0ea;content:"";pointer-events:none}.plot{position:relative;height:18px;overflow:hidden;border-radius:5px;background-color:#f1f4f8;background-image:linear-gradient(90deg,transparent calc(25% - .5px),#e0e6ef calc(25% - .5px),#e0e6ef calc(25% + .5px),transparent calc(25% + .5px),transparent calc(50% - .5px),#e0e6ef calc(50% - .5px),#e0e6ef calc(50% + .5px),transparent calc(50% + .5px),transparent calc(75% - .5px),#e0e6ef calc(75% - .5px),#e0e6ef calc(75% + .5px),transparent calc(75% + .5px))}.bar{position:absolute;top:3px;height:12px;min-width:4px;border:1px solid #4971e2;border-radius:4px;background:linear-gradient(90deg,#6c8cf0,#5278e7);box-shadow:0 2px 4px #5278e733}.bar.status-complete{border-color:#16836d;background:linear-gradient(90deg,#37b799,#16836d);box-shadow:0 2px 4px #16836d24}.bar.status-failed{border-color:#c6495a;background:linear-gradient(90deg,#ea8290,#c6495a);box-shadow:0 2px 4px #c6495a24}.bar.status-waiting{border-color:#be841e;background:linear-gradient(90deg,#edbd61,#c48b29);box-shadow:0 2px 4px #c48b2924}.bar.status-conflicted{border-color:#be841e;background:linear-gradient(90deg,#edbd61,#c48b29)}.bar.status-running{background:linear-gradient(90deg,#88a5ff,#5278e7);animation:pulse 1.8s ease-in-out infinite}.bar.is-group{opacity:.35;box-shadow:none;border-style:dashed;animation:none}@keyframes pulse{50%{opacity:.65}}.interval,.duration{text-align:right;color:#75839a;font-size:10px;font-variant-numeric:tabular-nums;white-space:nowrap}.duration{color:#33425d;font-weight:700}.tree-count{margin-left:auto;color:#8896ab;font-size:9px;white-space:nowrap}.empty{padding:42px 20px;text-align:center;color:var(--muted);font-size:12px}.footnote{margin:14px 2px 0;color:#8996aa;font-size:10px}.footer{display:flex;justify-content:space-between;gap:15px;margin:18px 2px 0;color:#96a1b3;font-size:10px}.footer strong{color:#65738a}@media(max-width:900px){.shell{padding:28px 20px 42px}.metrics{grid-template-columns:repeat(2,minmax(0,1fr))}.timeline{min-width:760px}.axis-row,.graph-row{grid-template-columns:minmax(190px,235px) minmax(270px,1fr) 92px 55px;gap:10px}}@media(max-width:560px){.shell{padding:20px 12px 32px}.topline{margin-bottom:26px}.snapshot{font-size:9px}.report-head{align-items:flex-start}.report-head h1{font-size:25px}.state{padding:6px 9px;font-size:10px}.metrics{gap:8px}.metric{padding:13px 12px}.metric strong{font-size:20px}.token-panel{padding:16px}.token-grid{grid-template-columns:repeat(2,minmax(0,1fr));row-gap:15px}.token-metric:nth-child(3){padding-left:0;border-left:0}.graph-head{padding:17px 16px 13px}.legend{gap:8px}.graph-scroll{padding:2px 12px 18px}.timeline{min-width:740px}.axis-row,.graph-row{grid-template-columns:180px minmax(265px,1fr) 85px 48px;gap:8px}.footer{flex-direction:column}}
</style></head><body><main class="shell"><div class="topline"><div class="brand"><span class="brand-mark" aria-hidden="true">A</span><span>Astra · Run insights</span></div><span class="snapshot">Offline snapshot · ${graph.nodes.length} stages</span></div><header class="report-head"><div><p class="eyebrow">Explain Analyze</p><h1>${title}</h1><p class="subtitle">A visual account of what ran, when it ran, and what overlapped.</p></div><span class="state ${statusClass}">${escapeHtml(statusLabel)}</span></header><section class="metrics" aria-label="Run measurements">${metricCards}</section><section class="panel token-panel" aria-labelledby="tokens-heading"><div class="panel-head"><div><h2 id="tokens-heading">Model token usage</h2><p>Values are attributed to individual physical model requests.</p></div><span class="source-tag">${providerAttempts.length > 0 && providerAttempts.every((node) => node.usage?.basis === "provider_exact") ? "Provider reported" : "Partial or unavailable"}</span></div><div class="token-grid">${tokenCards}</div><p class="footnote">Totals include every request, including failed attempts that used tokens. Unknown lanes stay unknown.</p></section><section class="panel graph-panel" aria-labelledby="graph-heading"><div class="graph-head"><div class="panel-head"><div><h2 id="graph-heading">Execution graph</h2><p>Time runs left to right. Solid bars are work; faded bars frame nested stages. Overlap means parallel work.</p></div></div><div class="legend" aria-label="Timeline status legend"><span class="legend-item"><i class="legend-dot complete"></i>Completed</span><span class="legend-item"><i class="legend-dot failed"></i>Failed</span><span class="legend-item"><i class="legend-dot running"></i>In progress</span><span class="legend-item"><i class="legend-dot waiting"></i>Waiting</span><span class="legend-item"><i class="bar-key group"></i>Parent stage</span><span class="legend-item"><i class="bar-key work"></i>Work stage</span></div></div>${warning}<div class="graph-scroll">${domains || "<div class=\"empty\">Execution facts will appear here as the run advances.</div>"}</div></section><footer class="footer"><span>Overlapping bars show parallel work; nested stages preserve execution structure.</span><strong>Explain Analyze · bounded runtime facts</strong></footer></main></body></html>`;
}

function renderHtmlTimelineDomain(
  nodes: readonly ExplainAnalyzeNodeV1[],
  domainIndex: number,
  domainCount: number,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
): string {
  const domainEnd = Math.max(
    1,
    ...nodes.map((node) => node.endElapsedMs ?? node.startElapsedMs + (node.durationMs ?? 0)),
  );
  const timelineNodes = new Map(nodes.map((node) => [node.nodeId, node]));
  const children = new Map<string, ExplainAnalyzeNodeV1[]>();
  for (const node of nodes) {
    if (!node.parentNodeId || !timelineNodes.has(node.parentNodeId)) continue;
    const group = children.get(node.parentNodeId) ?? [];
    group.push(node);
    children.set(node.parentNodeId, group);
  }
  const visited = new Set<string>();
  const tree = nodes
    .filter((node) => !node.parentNodeId || !timelineNodes.has(node.parentNodeId))
    .map((node) => renderHtmlTreeNode(node, children, nodeById, visited, domainEnd))
    .join("");
  const remaining = nodes
    .filter((node) => !visited.has(node.nodeId))
    .map((node) => renderHtmlTreeNode(node, children, nodeById, visited, domainEnd))
    .join("");
  const ticks = [0, 25, 50, 75, 100]
    .map((percent) => `<span class="axis-tick" style="left:${percent}%">${formatMs(domainEnd * percent / 100)}</span>`)
    .join("");
  const domainLabel = domainCount > 1 ? `Timeline ${domainIndex + 1}` : "Turn timeline";
  return `<section class="domain" aria-label="${domainLabel}"><h3 class="domain-label">${domainLabel}</h3><div class="timeline"><div class="axis-row"><span class="axis-side">Stage</span><div class="axis-track">${ticks}</div><span class="axis-side interval-col">Interval</span><span class="axis-side duration-col">Time</span></div><div class="graph-tree">${tree}${remaining}</div></div></section>`;
}

function renderHtmlTreeNode(
  node: ExplainAnalyzeNodeV1,
  childrenByParent: ReadonlyMap<string, readonly ExplainAnalyzeNodeV1[]>,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  visited: Set<string>,
  domainEnd: number,
  depth = 0,
): string {
  if (visited.has(node.nodeId)) return "";
  visited.add(node.nodeId);
  const children = (childrenByParent.get(node.nodeId) ?? []).filter(
    (child) => !visited.has(child.nodeId),
  );
  const row = renderHtmlNode(node, domainEnd, nodeById, children.length, depth);
  if (children.length === 0) return row;
  return `<details class="node-group" open>${row}<div class="node-children" style="--child-indent:${Math.min(depth, 4) * 14}px">${children.map((child) => renderHtmlTreeNode(child, childrenByParent, nodeById, visited, domainEnd, depth + 1)).join("")}</div></details>`;
}

function renderHtmlNode(
  node: ExplainAnalyzeNodeV1,
  domainEnd: number,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  childCount: number,
  depth: number,
): string {
  const numericStart = Math.max(0, Math.min(node.startElapsedMs, domainEnd));
  const end = Math.max(numericStart, Math.min(node.endElapsedMs ?? numericStart + (node.durationMs ?? 0), domainEnd));
  const left = (numericStart / domainEnd) * 100;
  const width = Math.max(((end - numericStart) / domainEnd) * 100, 0.4);
  const status = nodeStatus(node);
  const duration = node.durationMs === undefined ? "In progress" : formatMs(node.durationMs);
  const request = node.kind === "provider_attempt"
    ? `<span class="request-chip">${escapeHtml(requestIdentity(node))}</span>`
    : "";
  const usage = node.usage
    ? `<span class="node-usage" title="${escapeHtml(formatUsageDetail(node.usage))}">${escapeHtml(formatUsage(node.usage))}</span>`
    : "";
  const children = childCount > 0
    ? `<span class="toggle" aria-hidden="true">›</span><span class="tree-count">${childCount} ${childCount === 1 ? "stage" : "stages"}</span>`
    : `<span class="node-leaf-mark status-${status.className}" aria-hidden="true"></span>`;
  const dependencies = node.dependencyNodeIds
    .slice(0, 3)
    .map((dependencyId) => {
      const dependency = nodeById.get(dependencyId);
      if (!dependency) return escapeHtml(dependencyId);
      const request = dependency.kind === "provider_attempt"
        ? ` (${requestIdentity(dependency)})`
        : "";
      return escapeHtml(`${dependency.label}${request}`);
    });
  const dependencySummary = dependencies.length > 0
    ? `<div class="node-dependencies">After ${dependencies.map((label) => `“${label}”`).join(", ")}${node.dependencyNodeIds.length > dependencies.length ? ` and ${node.dependencyNodeIds.length - dependencies.length} more` : ""}</div>`
    : "";
  const start = formatMs(node.startElapsedMs);
  const endTime = node.endElapsedMs === undefined ? "Now" : formatMs(node.endElapsedMs);
  const rowTag = childCount > 0 ? "summary" : "div";
  return `<${rowTag} class="graph-row status-${status.className}${childCount > 0 ? " is-group" : ""}"><div class="node-copy${depth > 0 ? " nested" : ""}" style="--depth:${Math.min(depth, 4)};--indent:${Math.min(depth, 4) * 14}px"><div class="node-titleline">${children}<span class="node-title" title="${escapeHtml(node.label)}">${escapeHtml(node.label)}</span>${request}</div><div class="node-meta"><span class="node-status">${escapeHtml(status.label)}</span>${usage}</div>${dependencySummary}</div><div class="plot" aria-hidden="true"><span class="bar${childCount > 0 ? " is-group" : ""} status-${status.className}" style="left:${left.toFixed(3)}%;width:${width.toFixed(3)}%"></span></div><span class="interval interval-col">${start} – ${endTime}</span><strong class="duration duration-col">${duration}</strong></${rowTag}>`;
}

function nodeStatus(node: ExplainAnalyzeNodeV1): { label: string; className: string } {
  if (node.conflicted) return { label: "Conflicting facts", className: "conflicted" };
  if (!node.terminalObserved) return { label: "In progress", className: "running" };
  const outcome = node.outcome;
  if (outcome === "failed" || outcome === "interrupted") {
    return { label: humanOutcome(outcome), className: "failed" };
  }
  if (outcome === "waiting" || outcome === "blocked" || outcome === "deferred") {
    return { label: humanOutcome(outcome), className: "waiting" };
  }
  if (outcome === "succeeded" || outcome === "completed" || outcome === "resolved") {
    return { label: "Completed", className: "complete" };
  }
  return { label: humanOutcome(outcome ?? "unavailable"), className: "complete" };
}

function requestIdentity(node: ExplainAnalyzeNodeV1): string {
  const round = node.roundIndex === undefined ? null : node.roundIndex + 1;
  const attempt = node.attemptIndex === undefined ? null : node.attemptIndex + 1;
  if (round === null && attempt === null) return "Model request";
  if (round !== null && attempt !== null) return `Round ${round} · request ${attempt}`;
  return round !== null ? `Round ${round}` : `Request ${attempt}`;
}

function nodeDepth(
  node: ExplainAnalyzeNodeV1,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
): number {
  const seen = new Set([node.nodeId]);
  let current = node;
  let depth = 0;
  while (current.parentNodeId && depth < 8) {
    const parent = nodeById.get(current.parentNodeId);
    if (!parent || parent.clockDomainId !== node.clockDomainId || seen.has(parent.nodeId)) break;
    seen.add(parent.nodeId);
    current = parent;
    depth += 1;
  }
  return depth;
}

export function explainAnalyzeNodeIsActive(node: ExplainAnalyzeNodeV1): boolean {
  return !node.terminalObserved;
}

export function formatMs(ms: number): string {
  if (ms < 1_000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(Math.round(ms / 100) / 10).toFixed(1)} s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.floor((ms % 60_000) / 1_000);
  return seconds === 0 ? `${minutes} min` : `${minutes} min ${seconds} s`;
}

export function formatUsage(usage: ExplainAnalyzeUsageV1): string {
  const lanes = [
    ["in", usage.fresh_input_tokens],
    ["cache", usage.cache_read_tokens],
    ["write", usage.cache_creation_tokens],
    ["out", usage.output_tokens],
  ] as const;
  const details = lanes
    .flatMap(([name, count]) =>
      count === undefined ? [] : [`${name} ${count.toLocaleString()}`],
    );
  return `${details.join(" · ")}${usage.basis === "provider_partial" ? " · partial" : usage.basis === "runtime_estimated" ? " · estimated" : ""}`;
}

export function formatUsageDetail(usage: ExplainAnalyzeUsageV1): string {
  const lanes = [
    ["Fresh input", usage.fresh_input_tokens],
    ["Cache read", usage.cache_read_tokens],
    ["Cache creation", usage.cache_creation_tokens],
    ["Output", usage.output_tokens],
  ] as const;
  const details = lanes
    .flatMap(([name, count]) =>
      count === undefined ? [] : [`${name}: ${count.toLocaleString()}`],
    )
    .join(" · ");
  const basis = usage.basis === "provider_exact"
    ? "Provider reported"
    : usage.basis === "provider_partial"
      ? "Partial provider report"
      : "Runtime estimate";
  return `${details} (${basis})`;
}

function summarizeUsageLanes(
  nodes: readonly ExplainAnalyzeNodeV1[],
): Array<{ label: string; value: string | null }> {
  const lanes = [
    ["Fresh input", "fresh_input_tokens"],
    ["Cache read", "cache_read_tokens"],
    ["Cache created", "cache_creation_tokens"],
    ["Output", "output_tokens"],
  ] as const;
  return lanes.map(([label, key]) => {
    const values = nodes.map((node) => node.usage?.[key]);
    const value = nodes.length > 0 && values.every((lane) => lane !== undefined)
      ? values.reduce((sum, lane) => sum + BigInt(lane ?? 0), 0n).toLocaleString()
      : null;
    return { label, value };
  });
}

function humanOutcome(outcome: NonNullable<ExplainAnalyzeEventV1["outcome"]>) {
  const labels: Record<typeof outcome, string> = {
    completed: "Completed",
    succeeded: "Succeeded",
    failed: "Failed",
    cancelled: "Cancelled",
    interrupted: "Interrupted",
    blocked: "Blocked",
    waiting: "Waiting",
    rejected: "Rejected",
    reused: "Reused",
    suppressed: "Suppressed",
    deferred: "Deferred",
    resolved: "Resolved",
    fallback: "Fallback used",
    unavailable: "Unavailable",
    delegated: "Delegated",
  };
  return labels[outcome];
}

function sameStrings(left: readonly string[], right: readonly string[]) {
  return left.length === right.length && left.every((value, index) => value === right[index]);
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  if (isRecord(value)) {
    const pairs = Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${stableJson(value[key])}`);
    return `{${pairs.join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown, maxLength: number): value is string {
  return typeof value === "string" && value.trim().length > 0 && value.length <= maxLength;
}

function isNonNegativeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (character) => {
    switch (character) {
      case "&": return "&amp;";
      case "<": return "&lt;";
      case ">": return "&gt;";
      case '"': return "&quot;";
      default: return "&#39;";
    }
  });
}

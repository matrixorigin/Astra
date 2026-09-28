import {
  explainAnalyzeCoverageGapLabel,
  explainAnalyzeAuxiliaryUsageLines,
  explainAnalyzeAuxiliaryDetailsLines,
  formatMs,
  memorySelectionLines,
  reduceExplainAnalyzeEvents,
} from "./explain-analyze";
import type { ExplainAnalyzeNodeV1 } from "./explain-analyze";
import type {
  ExplainAnalyzeContextMetricsV1,
  ExplainAnalyzeContextSourceKindV1,
  ExplainAnalyzeDecisionDetailV1,
  ExplainAnalyzeOutcomeV1,
  ExplainAnalyzeUsageV1,
} from "./types";

const MAX_RENDERED_NODES = 4_096;
const MAX_RENDER_DEPTH = 128;
const MAX_RENDERED_DEPENDENCIES = 64;
const MAX_RENDERED_CHARACTERS = 256_000;

type RenderState = {
  characters: number;
  renderedNodes: number;
  truncated: boolean;
};

type TreeFrame = {
  node: ExplainAnalyzeNodeV1;
  depth: number;
  ancestorsHaveMore: readonly boolean[];
  isLast: boolean;
  isRoot: boolean;
};

/**
 * Render the canonical Explain Analyze graph as a copyable README-style
 * report. Every relationship in the report comes from the reduced graph:
 * parent edges form the tree and dependency edges are rendered by target
 * label. The traversal is iterative so a damaged or unusually deep history
 * cannot overflow the JavaScript call stack.
 */
export function renderExplainAnalyzeText(
  events: readonly unknown[],
  options: { degraded?: boolean } = {},
): string {
  const graph = reduceExplainAnalyzeEvents(events);
  const state: RenderState = { characters: 0, renderedNodes: 0, truncated: false };
  const lines: string[] = [];

  appendLine(lines, "# Explain Analyze", state);
  appendLine(lines, "", state);
  if (options.degraded) {
    appendLine(lines, "Incomplete observation: delivery gap. Some execution facts may be missing.", state);
  }
  appendLine(lines, `Structural integrity: ${graph.integrity}`, state);
  if (graph.diagnostics.length > 0) {
    appendLine(lines, `Recorded graph diagnostics: ${graph.diagnostics.length}`, state);
  }
  if (graph.duplicateEventCount > 0) {
    appendLine(lines, `Duplicate facts ignored: ${graph.duplicateEventCount}`, state);
  }
  if (graph.coverageGaps.length > 0) {
    appendLine(
      lines,
      `Not measured separately: ${graph.coverageGaps
        .map(explainAnalyzeCoverageGapLabel)
        .join(" · ")}`,
      state,
    );
  }

  for (const line of explainAnalyzeAuxiliaryUsageLines(graph)) appendLine(lines, cleanText(line), state);
  for (const line of explainAnalyzeAuxiliaryDetailsLines(graph)) appendLine(lines, cleanText(line), state);

  if (graph.nodes.length === 0) {
    appendLine(lines, "", state);
    appendLine(lines, "No execution facts recorded.", state);
    return lines.join("\n");
  }

  const byClockDomain = new Map<string, ExplainAnalyzeNodeV1[]>();
  const nodeById = new Map<string, ExplainAnalyzeNodeV1>();
  for (const node of graph.nodes) {
    const domainNodes = byClockDomain.get(node.clockDomainId) ?? [];
    domainNodes.push(node);
    byClockDomain.set(node.clockDomainId, domainNodes);
    nodeById.set(node.nodeId, node);
  }

  for (const [clockDomainId, domainNodes] of [...byClockDomain.entries()].sort(
    ([left], [right]) => left.localeCompare(right),
  )) {
    if (state.truncated) break;
    appendLine(lines, "", state);
    appendLine(lines, `## Clock domain: ${cleanText(clockDomainId)}`, state);
    renderClockDomain(lines, domainNodes, nodeById, state);
  }

  if (state.truncated && lines[lines.length - 1] !== "… text export truncated") {
    // Keep the truncation marker useful even when the character budget was
    // reached while writing a long label or dependency section.
    lines.push("… text export truncated");
  }
  return lines.join("\n");
}

function renderClockDomain(
  lines: string[],
  nodes: readonly ExplainAnalyzeNodeV1[],
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  state: RenderState,
) {
  const domainById = new Map(nodes.map((node) => [node.nodeId, node]));
  const childrenByParent = new Map<string, ExplainAnalyzeNodeV1[]>();
  for (const node of nodes) {
    if (!node.parentNodeId || !domainById.has(node.parentNodeId)) continue;
    const children = childrenByParent.get(node.parentNodeId) ?? [];
    children.push(node);
    childrenByParent.set(node.parentNodeId, children);
  }
  for (const children of childrenByParent.values()) children.sort(compareNodes);

  const orderedNodes = [...nodes].sort(compareNodes);
  const roots = orderedNodes.filter(
    (node) => !node.parentNodeId || !domainById.has(node.parentNodeId),
  );
  const visited = new Set<string>();
  const pending: TreeFrame[] = roots
    .slice()
    .reverse()
    .map((node) => ({
      node,
      depth: 0,
      ancestorsHaveMore: [],
      isLast: true,
      isRoot: true,
    }));

  // Continue with unvisited nodes after the normal roots. This makes a
  // parent cycle visible without following the cycle forever.
  while (!state.truncated) {
    while (pending.length > 0 && !state.truncated) {
      const frame = pending.pop();
      if (!frame || visited.has(frame.node.nodeId)) continue;
      visited.add(frame.node.nodeId);

      if (state.renderedNodes >= MAX_RENDERED_NODES) {
        appendLine(
          lines,
          `${treeConnector(frame)}… additional recorded stages omitted`,
          state,
        );
        state.truncated = true;
        break;
      }
      state.renderedNodes += 1;
      renderNode(lines, frame, nodeById, state);
      if (state.truncated) break;

      const children = (childrenByParent.get(frame.node.nodeId) ?? []).filter(
        (child) => !visited.has(child.nodeId),
      );
      if (children.length === 0) continue;
      if (frame.depth >= MAX_RENDER_DEPTH) {
        appendLine(
          lines,
          `${childConnector(frame, true)}… nested stages omitted at depth limit`,
          state,
        );
        markDescendantsVisited(children, childrenByParent, visited);
        continue;
      }
      for (let index = children.length - 1; index >= 0; index -= 1) {
        pending.push({
          node: children[index],
          depth: frame.depth + 1,
          ancestorsHaveMore: [...frame.ancestorsHaveMore, !frame.isLast],
          isLast: index === children.length - 1,
          isRoot: false,
        });
      }
    }

    if (state.truncated) break;
    const remaining = orderedNodes.find((node) => !visited.has(node.nodeId));
    if (!remaining) break;
    pending.push({
      node: remaining,
      depth: 0,
      ancestorsHaveMore: [],
      isLast: true,
      isRoot: true,
    });
  }
}

function renderNode(
  lines: string[],
  frame: TreeFrame,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  state: RenderState,
) {
  const { node } = frame;
  const duration = node.durationMs === undefined ? "unknown" : formatMs(node.durationMs);
  const outcome = node.conflicted
    ? "unknown (conflicting facts)"
    : node.outcome === undefined
      ? "unknown"
      : humanOutcome(node.outcome);
  appendLine(
    lines,
    `${treeConnector(frame)}${cleanText(node.label)} · ${duration} · ${outcome}`,
    state,
  );
  if (state.truncated) return;

  const detailPrefix = detailConnector(frame);
  if (node.decisionDetail) {
    appendLine(
      lines,
      `${detailPrefix}${explainAnalyzeDecisionDetailLine(node.decisionDetail)}`,
      state,
    );
    if (state.truncated) return;
  }
  for (const dependencyId of node.dependencyNodeIds.slice(0, MAX_RENDERED_DEPENDENCIES)) {
    const dependency = nodeById.get(dependencyId);
    appendLine(
      lines,
      `${detailPrefix}depends on (recorded): ${dependency ? quoteLabel(dependency.label) : "unknown"}`,
      state,
    );
    if (state.truncated) return;
  }
  if (node.dependencyNodeIds.length > MAX_RENDERED_DEPENDENCIES) {
    appendLine(
      lines,
      `${detailPrefix}depends on (recorded): … ${node.dependencyNodeIds.length - MAX_RENDERED_DEPENDENCIES} more`,
      state,
    );
    if (state.truncated) return;
  }

  if (node.usage) {
    renderUsage(lines, node.usage, detailPrefix, state);
    if (state.truncated) return;
  }
  if (node.context) renderContext(lines, node.context, detailPrefix, state);
  if (state.truncated) return;

  if (node.coverageGaps.length > 0) {
    appendLine(
      lines,
      `${detailPrefix}· not measured separately: ${node.coverageGaps
        .map(explainAnalyzeCoverageGapLabel)
        .join(" · ")}`,
      state,
    );
  }
}

export function explainAnalyzeDecisionDetailLine(
  detail: ExplainAnalyzeDecisionDetailV1,
): string {
  const requirement = detail.requirement_index + 1;
  if (detail.match_count === 0) {
    return `Requested model requirement ${requirement}: no active authorized Chat-capable model exactly matched in the current catalog snapshot; no child was started.`;
  }
  return `Requested model requirement ${requirement}: ${detail.match_count} active authorized Chat-capable catalog entries matched exactly; selection was ambiguous, so no child was started.`;
}

function renderUsage(
  lines: string[],
  usage: ExplainAnalyzeUsageV1,
  detailPrefix: string,
  state: RenderState,
) {
  appendLine(lines, `${detailPrefix}· token usage (basis: ${usage.basis}):`, state);
  if (state.truncated) return;
  const lanes: Array<[string, number | undefined]> = [
    ["fresh input", usage.fresh_input_tokens],
    ["cache read", usage.cache_read_tokens],
    ["cache creation", usage.cache_creation_tokens],
    ["output", usage.output_tokens],
  ];
  for (const [label, count] of lanes) {
    appendLine(
      lines,
      `${detailPrefix}  ${label}: ${count === undefined ? "unknown" : `${formatCount(count)} tokens`}`,
      state,
    );
    if (state.truncated) return;
  }
}

function renderContext(
  lines: string[],
  context: ExplainAnalyzeContextMetricsV1,
  detailPrefix: string,
  state: RenderState,
) {
  if (context.budget) {
    const budget = context.budget;
    appendLine(lines, `${detailPrefix}· context estimate (pre_provider_estimate):`, state);
    if (state.truncated) return;
    const rows: Array<[string, number]> = [
      ["estimated input", budget.estimated_input_tokens],
      ["estimated system", budget.estimated_system_tokens],
      ["tool schemas", budget.tool_schema_tokens],
      ["requested output", budget.requested_output_tokens],
      ["reserved protocol", budget.reserved_protocol_tokens],
      ["effective input limit", budget.effective_input_limit_tokens],
      ["model context limit", budget.model_context_limit_tokens],
    ];
    for (const [label, count] of rows) {
      appendLine(
        lines,
        `${detailPrefix}  ${label}: ${formatCount(count)} tokens`,
        state,
      );
      if (state.truncated) return;
    }
    appendLine(
      lines,
      `${detailPrefix}  visible tools: ${formatCount(budget.visible_tool_count)}`,
      state,
    );
  }
  if (state.truncated || !context.assembly) return;

  const assembly = context.assembly;
  for (const report of assembly.edge_memory_selection ?? []) {
    for (const line of memorySelectionLines(report)) {
      appendLine(lines, `${detailPrefix}${line}`, state);
      if (state.truncated) return;
    }
  }
  appendLine(lines, `${detailPrefix}· context estimate (runtime_text_estimate):`, state);
  if (state.truncated) return;
  for (const source of assembly.sources) {
    const sourceLabel = contextSourceLabel(source.kind);
    const sections = `${source.section_count} ${source.section_count === 1 ? "section" : "sections"}`;
    appendLine(
      lines,
      `${detailPrefix}  ${sourceLabel}: ${formatCount(source.estimated_tokens)} tokens (${sections})`,
      state,
    );
    if (state.truncated) return;
  }
}

function markDescendantsVisited(
  roots: readonly ExplainAnalyzeNodeV1[],
  childrenByParent: ReadonlyMap<string, readonly ExplainAnalyzeNodeV1[]>,
  visited: Set<string>,
) {
  const pending = [...roots];
  while (pending.length > 0) {
    const node = pending.pop();
    if (!node || visited.has(node.nodeId)) continue;
    visited.add(node.nodeId);
    pending.push(...(childrenByParent.get(node.nodeId) ?? []));
  }
}

function compareNodes(left: ExplainAnalyzeNodeV1, right: ExplainAnalyzeNodeV1): number {
  return (
    left.startElapsedMs - right.startElapsedMs ||
    (left.endElapsedMs ?? Number.MAX_SAFE_INTEGER) -
      (right.endElapsedMs ?? Number.MAX_SAFE_INTEGER) ||
    left.nodeId.localeCompare(right.nodeId)
  );
}

function appendLine(lines: string[], line: string, state: RenderState): boolean {
  if (state.truncated) return false;
  const extra = line.length + (lines.length > 0 ? 1 : 0);
  if (state.characters + extra > MAX_RENDERED_CHARACTERS) {
    state.truncated = true;
    return false;
  }
  lines.push(line);
  state.characters += extra;
  return true;
}

function treeConnector(frame: TreeFrame): string {
  if (frame.isRoot) return "";
  return childConnector(frame, frame.isLast);
}

function childConnector(frame: TreeFrame, lastChild: boolean): string {
  const prefix = frame.ancestorsHaveMore.map((hasMore) => (hasMore ? "│  " : "   ")).join("");
  return `${prefix}${lastChild ? "└─ " : "├─ "}`;
}

function detailConnector(frame: TreeFrame): string {
  if (frame.isRoot) return "  ";
  const prefix = frame.ancestorsHaveMore.map((hasMore) => (hasMore ? "│  " : "   ")).join("");
  return `${prefix}${frame.isLast ? "   " : "│  "}  `;
}

function formatCount(count: number): string {
  return count.toLocaleString("en-US");
}

function quoteLabel(label: string): string {
  return `"${cleanText(label)}"`;
}

function cleanText(value: string): string {
  const cleaned = value.replace(/[\u0000-\u001f\u007f]/g, " ").replace(/\s+/g, " ").trim();
  return cleaned.length > 0 ? cleaned : "unknown";
}

function contextSourceLabel(kind: ExplainAnalyzeContextSourceKindV1): string {
  const labels: Record<ExplainAnalyzeContextSourceKindV1, string> = {
    identity: "Agent instructions",
    self_model: "Capabilities",
    project_context: "Project guidance",
    deferred_tools: "Deferred tools",
    available_skills: "Skill catalog",
    memory: "Retrieved memory",
    working_memory: "Working memory",
    history: "Conversation history",
    constraints: "Response constraints",
    skills: "Active skills",
    runtime_identity: "Runtime context",
    runtime_volatile: "Turn instructions",
    emergent_skills: "Discovered skills",
    emergent_memory: "Prefetched memory",
    emergent_summary: "Tool summaries",
  };
  return labels[kind];
}

function humanOutcome(outcome: ExplainAnalyzeOutcomeV1): string {
  const labels: Record<ExplainAnalyzeOutcomeV1, string> = {
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

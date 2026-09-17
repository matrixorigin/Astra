import type { ExplainAnalyzeNodeV1 } from "./explain-analyze";

/** A bounded, deterministic position for one Explain Analyze node. */
export type ExplainAnalyzeLayoutNodeV1 = {
  nodeId: string;
  x: number;
  y: number;
  width: number;
  height: number;
};

export type ExplainAnalyzeLayoutEdgeKindV1 = "parent" | "dependency";

/**
 * A rendered connector.  Paths are in the domain's local SVG coordinate
 * system and deliberately contain no event-derived relationships of their
 * own: every path corresponds to a parent or dependency id from the input.
 */
export type ExplainAnalyzeLayoutEdgeV1 = {
  sourceNodeId: string;
  targetNodeId: string;
  kind: ExplainAnalyzeLayoutEdgeKindV1;
  path: string;
};

export type ExplainAnalyzeLayoutDomainV1 = {
  clockDomainId: string;
  width: number;
  height: number;
  nodes: ExplainAnalyzeLayoutNodeV1[];
  edges: ExplainAnalyzeLayoutEdgeV1[];
};

export type ExplainAnalyzeLayoutV1 = ExplainAnalyzeLayoutDomainV1[];

export type ExplainAnalyzeLayoutOptionsV1 = {
  /** Keep the layout bounded when a caller supplies an unwindowed graph. */
  maxNodes?: number;
  /** Retained as a density hint for callers; LR layout stacks siblings vertically. */
  maxLanes?: number;
};

const DEFAULT_MAX_NODES = 500;
const ABSOLUTE_MAX_NODES = 2_000;
const DEFAULT_MAX_LANES = 4;
const CARD_WIDTH = 236;
const CARD_MIN_HEIGHT = 96;
const CARD_MAX_HEIGHT = 220;
const COLUMN_GAP = 40;
const ROW_GAP = 34;
const PADDING_X = 24;
const PADDING_Y = 24;

/**
 * Produce a stable left-to-right DAG layout for Explain Analyze facts.
 *
 * Parent edges express containment and dependency edges express an observed
 * prerequisite.  No edge is inferred from timestamps, sibling order, or
 * shared parents.  Facts from another clock domain are kept in that domain,
 * and malformed references are omitted from the drawable edge set.
 *
 * The layout keeps each containment level in a column and stacks sibling
 * subtrees vertically. This keeps a large fan-out from making the canvas wide
 * while preserving a readable root-to-leaf direction. Cyclic edges are
 * excluded from placement and drawing so a
 * malformed delivery cannot recurse forever or make the graph imply a valid
 * causal direction.
 */
export function layoutExplainAnalyzeGraph(
  nodes: readonly ExplainAnalyzeNodeV1[],
  options: ExplainAnalyzeLayoutOptionsV1 = {},
): ExplainAnalyzeLayoutV1 {
  const maxNodes = clampInteger(options.maxNodes ?? DEFAULT_MAX_NODES, 1, ABSOLUTE_MAX_NODES);
  const maxLanes = clampInteger(options.maxLanes ?? DEFAULT_MAX_LANES, 1, 8);
  const byClock = new Map<string, ExplainAnalyzeNodeV1[]>();

  // The reducer already emits deterministic node order. Preserve that order
  // for a stable UI, while de-duplicating defensively for direct SDK callers.
  for (const node of nodes) {
    if (!byClock.has(node.clockDomainId)) byClock.set(node.clockDomainId, []);
    const group = byClock.get(node.clockDomainId)!;
    if (group.length < maxNodes && !group.some((candidate) => candidate.nodeId === node.nodeId)) {
      group.push(node);
    }
  }

  return [...byClock.entries()].map(([clockDomainId, domainNodes]) =>
    layoutDomain(clockDomainId, domainNodes, maxLanes));
}

function layoutDomain(
  clockDomainId: string,
  nodes: readonly ExplainAnalyzeNodeV1[],
  maxLanes: number,
): ExplainAnalyzeLayoutDomainV1 {
  void maxLanes;
  const nodeById = new Map(nodes.map((node) => [node.nodeId, node]));
  const edges = collectEdges(nodes, nodeById);
  const cyclicEdges = findCyclicEdges(edges);
  const acyclicEdges = edges.filter((edge) => !cyclicEdges.has(edgeKey(edge)));
  const parentEdges = acyclicEdges.filter((edge) => edge.kind === "parent");
  const childrenByParent = new Map<string, ExplainAnalyzeNodeV1[]>();
  const childIds = new Set<string>();
  for (const edge of parentEdges) {
    const child = nodeById.get(edge.targetNodeId);
    if (!child) continue;
    const children = childrenByParent.get(edge.sourceNodeId) ?? [];
    children.push(child);
    childrenByParent.set(edge.sourceNodeId, children);
    childIds.add(child.nodeId);
  }
  for (const children of childrenByParent.values()) children.sort(compareNodes);

  const measuring = new Set<string>();
  const measured = new Map<string, TreeMeasure>();
  const measure = (node: ExplainAnalyzeNodeV1): TreeMeasure => {
    const cached = measured.get(node.nodeId);
    if (cached) return cached;
    // A malformed cycle should remain a visible node without causing a
    // recursive layout. Its cyclic edge has already been excluded above.
    if (measuring.has(node.nodeId)) {
      return { node, width: CARD_WIDTH, height: cardHeight(node), children: [] };
    }
    measuring.add(node.nodeId);
    const children = (childrenByParent.get(node.nodeId) ?? []).map(measure);
    const childrenWidth = children.reduce((largest, child) => Math.max(largest, child.width), 0);
    const childrenHeight = children.reduce((total, child) => total + child.height, 0) +
      Math.max(0, children.length - 1) * ROW_GAP;
    const result: TreeMeasure = {
      node,
      width: CARD_WIDTH + (children.length > 0 ? COLUMN_GAP + childrenWidth : 0),
      height: Math.max(cardHeight(node), childrenHeight),
      children,
    };
    measured.set(node.nodeId, result);
    measuring.delete(node.nodeId);
    return result;
  };
  const roots = nodes.filter((node) => !childIds.has(node.nodeId)).sort(compareNodes).map(measure);
  // Every node should be reachable from a root after cycle filtering. Keep a
  // malformed disconnected remainder visible as an additional root.
  const rootIds = new Set(roots.map((root) => root.node.nodeId));
  for (const node of nodes) {
    if (!measured.has(node.nodeId)) {
      const remainder = measure(node);
      if (!rootIds.has(node.nodeId)) {
        roots.push(remainder);
        rootIds.add(node.nodeId);
      }
    }
  }
  const positions = new Map<string, ExplainAnalyzeLayoutNodeV1>();
  let y = PADDING_Y;
  let maxRootWidth = CARD_WIDTH;
  const place = (tree: TreeMeasure, x: number, treeY: number) => {
    const nodeHeight = cardHeight(tree.node);
    const nodeY = treeY + (tree.height - nodeHeight) / 2;
    positions.set(tree.node.nodeId, {
      nodeId: tree.node.nodeId,
      // A left-to-right tree advances one card column at a time. Centering a
      // parent in its subtree would move it into the child columns and make
      // the containment edge appear to travel backwards through cards.
      x,
      y: nodeY,
      width: CARD_WIDTH,
      height: nodeHeight,
    });
    if (tree.children.length === 0) return;
    const childrenHeight = tree.children.reduce((total, child) => total + child.height, 0) +
      Math.max(0, tree.children.length - 1) * ROW_GAP;
    let childY = treeY + (tree.height - childrenHeight) / 2;
    const childX = x + CARD_WIDTH + COLUMN_GAP;
    for (const child of tree.children) {
      place(child, childX, childY);
      childY += child.height + ROW_GAP;
    }
  };
  for (const root of roots) {
    maxRootWidth = Math.max(maxRootWidth, root.width);
    place(root, PADDING_X, y);
    y += root.height + ROW_GAP;
  }
  const width = PADDING_X * 2 + maxRootWidth;
  const height = Math.max(PADDING_Y * 2 + CARD_MIN_HEIGHT, y - ROW_GAP + PADDING_Y);
  return {
    clockDomainId,
    width,
    height,
    nodes: nodes.map((node) => positions.get(node.nodeId)!).filter(Boolean),
    edges: acyclicEdges
      .map((edge) => {
        const source = positions.get(edge.sourceNodeId);
        const target = positions.get(edge.targetNodeId);
        if (!source || !target) return null;
        return { ...edge, path: connectorPath(source, target, edge.kind, width) };
      })
      .filter((edge): edge is ExplainAnalyzeLayoutEdgeV1 => edge !== null),
  };
}

type TreeMeasure = {
  node: ExplainAnalyzeNodeV1;
  width: number;
  height: number;
  children: TreeMeasure[];
}

function cardHeight(node: ExplainAnalyzeNodeV1) {
  // The card title is allowed to wrap. This estimate mirrors the graph card's
  // 236px width and 13px text size closely enough to keep long labels inside
  // their measured box without making ordinary cards unnecessarily tall.
  const titleUnits = Array.from(node.label).reduce(
    (sum, character) => sum + (character.codePointAt(0)! > 0xff ? 2 : 1),
    0,
  );
  const titleLines = Math.max(1, Math.ceil(titleUnits / 29));
  const hasDetail = node.usage !== undefined || node.context !== undefined;
  const height = 60 + titleLines * 18 + (hasDetail ? 28 : 0);
  return Math.min(CARD_MAX_HEIGHT, Math.max(CARD_MIN_HEIGHT, height));
}

type LayoutEdge = Omit<ExplainAnalyzeLayoutEdgeV1, "path">;

function collectEdges(
  nodes: readonly ExplainAnalyzeNodeV1[],
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
): LayoutEdge[] {
  const edges: LayoutEdge[] = [];
  const seen = new Set<string>();
  for (const node of nodes) {
    if (node.parentNodeId && node.parentNodeId !== node.nodeId) {
      addEdge(edges, seen, node.parentNodeId, node.nodeId, "parent", nodeById);
    }
    for (const dependencyNodeId of node.dependencyNodeIds) {
      if (dependencyNodeId !== node.nodeId) {
        addEdge(edges, seen, dependencyNodeId, node.nodeId, "dependency", nodeById);
      }
    }
  }
  return edges;
}

function addEdge(
  edges: LayoutEdge[],
  seen: Set<string>,
  sourceNodeId: string,
  targetNodeId: string,
  kind: ExplainAnalyzeLayoutEdgeKindV1,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
) {
  const source = nodeById.get(sourceNodeId);
  const target = nodeById.get(targetNodeId);
  if (!source || !target || source.clockDomainId !== target.clockDomainId) return;
  const key = `${kind}\u0000${sourceNodeId}\u0000${targetNodeId}`;
  if (seen.has(key)) return;
  seen.add(key);
  edges.push({ sourceNodeId, targetNodeId, kind });
}

function compareNodes(left: ExplainAnalyzeNodeV1, right: ExplainAnalyzeNodeV1) {
  return left.startElapsedMs - right.startElapsedMs ||
    left.nodeId.localeCompare(right.nodeId);
}

function edgeKey(edge: LayoutEdge) {
  return `${edge.kind}\u0000${edge.sourceNodeId}\u0000${edge.targetNodeId}`;
}

function findCyclicEdges(edges: readonly LayoutEdge[]) {
  const adjacency = new Map<string, string[]>();
  for (const edge of edges) {
    const targets = adjacency.get(edge.sourceNodeId) ?? [];
    targets.push(edge.targetNodeId);
    adjacency.set(edge.sourceNodeId, targets);
  }
  const indexByNode = new Map<string, number>();
  const lowByNode = new Map<string, number>();
  const stack: string[] = [];
  const onStack = new Set<string>();
  const componentByNode = new Map<string, number>();
  const componentSizes = new Map<number, number>();
  let nextIndex = 0;
  let nextComponent = 0;
  const visit = (nodeId: string) => {
    indexByNode.set(nodeId, nextIndex);
    lowByNode.set(nodeId, nextIndex);
    nextIndex += 1;
    stack.push(nodeId);
    onStack.add(nodeId);
    for (const target of adjacency.get(nodeId) ?? []) {
      if (!indexByNode.has(target)) {
        visit(target);
        lowByNode.set(nodeId, Math.min(lowByNode.get(nodeId)!, lowByNode.get(target)!));
      } else if (onStack.has(target)) {
        lowByNode.set(nodeId, Math.min(lowByNode.get(nodeId)!, indexByNode.get(target)!));
      }
    }
    if (lowByNode.get(nodeId) !== indexByNode.get(nodeId)) return;
    const component: string[] = [];
    let member: string | undefined;
    do {
      member = stack.pop();
      if (member === undefined) break;
      onStack.delete(member);
      component.push(member);
    } while (member !== nodeId);
    const componentId = nextComponent;
    nextComponent += 1;
    componentSizes.set(componentId, component.length);
    component.forEach((item) => componentByNode.set(item, componentId));
  };
  for (const nodeId of new Set(edges.flatMap((edge) => [edge.sourceNodeId, edge.targetNodeId]))) {
    if (!indexByNode.has(nodeId)) visit(nodeId);
  }
  return new Set(edges.filter((edge) => {
    const sourceComponent = componentByNode.get(edge.sourceNodeId);
    const targetComponent = componentByNode.get(edge.targetNodeId);
    return sourceComponent !== undefined && sourceComponent === targetComponent &&
      (componentSizes.get(sourceComponent) ?? 0) > 1;
  }).map(edgeKey));
}

function connectorPath(
  source: ExplainAnalyzeLayoutNodeV1,
  target: ExplainAnalyzeLayoutNodeV1,
  kind: ExplainAnalyzeLayoutEdgeKindV1,
  domainWidth: number,
) {
  const sourceRightX = source.x + source.width;
  const targetLeftX = target.x;
  const sourceY = source.y + source.height / 2;
  const targetY = target.y + target.height / 2;
  if (kind === "parent") {
    const delta = targetLeftX - sourceRightX;
    if (delta > 0) {
      const bend = Math.max(28, delta * 0.42);
      return `M ${round(sourceRightX)} ${round(sourceY)} C ${round(sourceRightX + bend)} ${round(sourceY)}, ${round(targetLeftX - bend)} ${round(targetY)}, ${round(targetLeftX)} ${round(targetY)}`;
    }
    // A malformed backward containment fact remains visible through a side
    // gutter, without crossing cards or changing its recorded direction.
    return gutterPath(source, target, domainWidth, true);
  }
  // Dependencies use an outer gutter and a side entry, making them visually
  // distinct from containment while keeping the line outside unrelated cards.
  return gutterPath(source, target, domainWidth, true);
}

function gutterPath(
  source: ExplainAnalyzeLayoutNodeV1,
  target: ExplainAnalyzeLayoutNodeV1,
  domainWidth: number,
  sideEntry: boolean,
) {
  const direction = target.x >= source.x ? 1 : -1;
  const sourceSideX = source.x + (direction > 0 ? source.width : 0);
  const targetSideX = target.x + (direction > 0 ? target.width : 0);
  const gutter = direction > 0 ? domainWidth - PADDING_X / 2 : PADDING_X / 2;
  const sourceY = source.y + source.height / 2;
  const targetY = target.y + target.height / 2;
  const bend = Math.max(42, Math.abs(gutter - sourceSideX) * 0.28);
  const verticalBend = Math.max(24, Math.abs(targetY - sourceY) * 0.32);
  const sourceAnchorX = sideEntry ? sourceSideX : source.x + source.width / 2;
  const targetAnchorX = sideEntry ? targetSideX : target.x + target.width / 2;
  return `M ${round(sourceAnchorX)} ${round(sourceY)} C ${round(sourceSideX + direction * bend)} ${round(sourceY)}, ${round(gutter - direction * bend)} ${round(sourceY)}, ${round(gutter)} ${round(sourceY)} C ${round(gutter)} ${round(sourceY + verticalBend)}, ${round(gutter)} ${round(targetY - verticalBend)}, ${round(gutter)} ${round(targetY)} C ${round(gutter - direction * bend)} ${round(targetY)}, ${round(targetSideX + direction * bend)} ${round(targetY)}, ${round(targetAnchorX)} ${round(targetY)}`;
}

function round(value: number) {
  return Math.round(value * 10) / 10;
}

function clampInteger(value: number, minimum: number, maximum: number) {
  return Number.isFinite(value)
    ? Math.min(maximum, Math.max(minimum, Math.floor(value)))
    : minimum;
}

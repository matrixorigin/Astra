import { describe, expect, it } from "vitest";
import type { ExplainAnalyzeNodeV1 } from "../explain-analyze";
import { layoutExplainAnalyzeGraph } from "../explain-analyze-layout";

const identity = {
  runId: "run",
  turnId: "turn",
  clockDomainId: "clock",
  kind: "tool_call" as const,
  coverageGaps: [],
  startObserved: true,
  terminalObserved: true,
  conflicted: false,
};

function node(
  nodeId: string,
  startElapsedMs: number,
  extra: Partial<ExplainAnalyzeNodeV1> = {},
): ExplainAnalyzeNodeV1 {
  return {
    ...identity,
    nodeId,
    label: nodeId,
    dependencyNodeIds: [],
    startElapsedMs,
    endElapsedMs: startElapsedMs + 10,
    durationMs: 10,
    ...extra,
  };
}

describe("layoutExplainAnalyzeGraph", () => {
  it("draws only explicit containment and dependency relationships", () => {
    const facts = [
      node("turn", 0, { kind: "turn" }),
      node("read-a", 10, { parentNodeId: "turn" }),
      node("read-b", 20, { parentNodeId: "turn" }),
      node("merge", 30, { parentNodeId: "turn", dependencyNodeIds: ["read-a", "read-b"] }),
      node("unrelated", 40),
    ];
    const [domain] = layoutExplainAnalyzeGraph(facts);
    expect(domain).toBeDefined();
    const relationshipKeys = domain.edges.map((edge) => `${edge.kind}:${edge.sourceNodeId}->${edge.targetNodeId}`);
    expect(relationshipKeys).toEqual([
      "parent:turn->read-a",
      "parent:turn->read-b",
      "parent:turn->merge",
      "dependency:read-a->merge",
      "dependency:read-b->merge",
    ]);
    expect(relationshipKeys.some((key) => key.includes("read-a->read-b"))).toBe(false);
    expect(domain.edges.every((edge) => edge.path.startsWith("M "))).toBe(true);

    const positions = new Map(domain.nodes.map((position) => [position.nodeId, position]));
    const turn = positions.get("turn")!;
    const left = positions.get("read-a")!;
    const right = positions.get("read-b")!;
    const merge = positions.get("merge")!;
    const childBlockCenter = (left.y + merge.y + merge.height) / 2;
    expect(turn.x + turn.width).toBeLessThan(left.x);
    expect(left.x).toBe(right.x);
    expect(left.y).toBeLessThan(right.y);
    expect(turn.y + turn.height / 2).toBe(childBlockCenter);
  });

  it("advances containment columns without shrinking the readable card width", () => {
    const [domain] = layoutExplainAnalyzeGraph([
      node("root", 0),
      node("batch", 10, { parentNodeId: "root" }),
      node("admission", 20, { parentNodeId: "batch" }),
      node("approval", 30, { parentNodeId: "admission" }),
    ]);
    const positions = new Map(domain.nodes.map((position) => [position.nodeId, position]));
    expect(domain.width).toBe(1_112);
    expect(domain.nodes.every((position) => position.width === 236)).toBe(true);
    expect(positions.get("root")!.x).toBe(24);
    expect(positions.get("batch")!.x).toBe(300);
    expect(positions.get("admission")!.x).toBe(576);
    expect(positions.get("approval")!.x).toBe(852);
    expect(new Set(domain.nodes.map((position) => position.y)).size).toBe(1);
  });

  it("keeps clock domains isolated, bounds nodes, and survives cycles", () => {
    const facts = [
      node("a", 0, { parentNodeId: "b" }),
      node("b", 10, { parentNodeId: "a" }),
      node("after", 20, { parentNodeId: "a" }),
      node("foreign", 0, { clockDomainId: "other", dependencyNodeIds: ["a"] }),
      node("extra", 20),
    ];
    const domains = layoutExplainAnalyzeGraph(facts, { maxNodes: 4 });
    expect(domains).toHaveLength(2);
    expect(domains[0].nodes.map((item) => item.nodeId)).toEqual(["a", "b", "after", "extra"]);
    expect(domains[0].edges.map((edge) => `${edge.kind}:${edge.sourceNodeId}->${edge.targetNodeId}`)).toEqual([
      "parent:a->after",
    ]);
    expect(domains[1].edges).toEqual([]);
    expect(domains[1].nodes.map((item) => item.nodeId)).toEqual(["foreign"]);
    expect(domains.every((domain) => domain.width < 2_000 && domain.height < 2_000)).toBe(true);
  });

  it("sizes a card for a long label instead of truncating it", () => {
    const [domain] = layoutExplainAnalyzeGraph([
      node("long", 0, { label: "A stage with a deliberately long label that should remain readable in the graph card" }),
    ]);
    expect(domain.nodes[0].height).toBeGreaterThan(96);
  });
});

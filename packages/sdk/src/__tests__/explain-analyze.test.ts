import { describe, expect, it } from "vitest";
import {
  explainAnalyzeMaxConcurrency,
  isExplainAnalyzeEventV1,
  reduceExplainAnalyzeEvents,
  renderExplainAnalyzeHtml,
} from "../explain-analyze";
import type { ExplainAnalyzeEventV1 } from "../types";

const identity = {
  type: "explain_analyze" as const,
  schema_version: 1 as const,
  run_id: "run-1",
  turn_id: "turn-1",
  producer_id: "worker-1",
  clock_domain_id: "worker-1/turn-1",
};

function started(
  nodeId: string,
  kind: ExplainAnalyzeEventV1["kind"],
  elapsedMs: number,
  extra: Partial<ExplainAnalyzeEventV1> = {},
): ExplainAnalyzeEventV1 {
  return {
    ...identity,
    event_id: `${nodeId}:start`,
    node_id: nodeId,
    kind,
    label: nodeId,
    transition: "started",
    elapsed_ms: elapsedMs,
    ...extra,
  };
}

function finished(
  nodeId: string,
  kind: ExplainAnalyzeEventV1["kind"],
  startMs: number,
  endMs: number,
  extra: Partial<ExplainAnalyzeEventV1> = {},
): ExplainAnalyzeEventV1 {
  return {
    ...identity,
    event_id: `${nodeId}:finish`,
    node_id: nodeId,
    kind,
    label: nodeId,
    transition: "finished",
    elapsed_ms: endMs,
    start_elapsed_ms: startMs,
    duration_ms: endMs - startMs,
    outcome: "succeeded",
    ...extra,
  };
}

describe("Explain Analyze graph reducer", () => {
  it("rebuilds a missing start, deduplicates replay, and measures same-clock overlap", () => {
    const first = finished("attempt-0", "provider_attempt", 10, 80, {
      parent_node_id: "round-0",
      round_index: 0,
      attempt_index: 0,
      usage: {
        basis: "provider_partial",
        fresh_input_tokens: 42,
        cache_read_tokens: 0,
        output_tokens: 9,
      },
    });
    const secondStart = started("attempt-1", "provider_attempt", 40, {
      parent_node_id: "round-0",
      round_index: 0,
      attempt_index: 1,
    });
    const secondFinish = finished("attempt-1", "provider_attempt", 40, 90, {
      parent_node_id: "round-0",
      round_index: 0,
      attempt_index: 1,
    });
    const graph = reduceExplainAnalyzeEvents([
      first,
      first,
      { ...first, index: 12 },
      secondStart,
      secondFinish,
      finished("round-0", "model_round", 0, 100, { round_index: 0 }),
    ]);

    expect(graph.duplicateEventCount).toBe(2);
    expect(graph.conflictedNodeIds).toEqual([]);
    expect(graph.nodes.find((node) => node.nodeId === "attempt-0")).toMatchObject({
      nodeId: "attempt-0",
      startElapsedMs: 10,
      startObserved: false,
      terminalObserved: true,
      usage: {
        fresh_input_tokens: 42,
        cache_read_tokens: 0,
        output_tokens: 9,
      },
    });
    expect(explainAnalyzeMaxConcurrency(graph)).toBe(2);
  });

  it("rejects a self-contradictory duration and reports conflicting node facts", () => {
    const malformed = finished("attempt-0", "provider_attempt", 10, 80, {
      round_index: 0,
      attempt_index: 0,
      duration_ms: 1,
    });
    expect(isExplainAnalyzeEventV1(malformed)).toBe(false);

    const graph = reduceExplainAnalyzeEvents([
      started("stage", "preparation", 10),
      started("stage", "preparation", 20, { event_id: "conflicting-start" }),
    ]);
    expect(graph.conflictedNodeIds).toEqual(["stage"]);
  });

  it("detects contradictory start offsets and attempt identity in either arrival order", () => {
    const start = started("attempt", "provider_attempt", 10, {
      round_index: 0,
      attempt_index: 0,
    });
    const finish = finished("attempt", "provider_attempt", 15, 30, {
      round_index: 1,
      attempt_index: 0,
    });

    expect(reduceExplainAnalyzeEvents([start, finish]).conflictedNodeIds).toEqual([
      "attempt",
    ]);
    expect(reduceExplainAnalyzeEvents([finish, start]).conflictedNodeIds).toEqual([
      "attempt",
    ]);
  });

  it("keeps a parent before its child when both start at the same time", () => {
    const graph = reduceExplainAnalyzeEvents([
      started("turn", "turn", 0, { label: "Turn" }),
      started("stage", "preparation", 0, {
        parent_node_id: "turn",
        label: "Preparation",
      }),
      finished("turn", "turn", 0, 20, { label: "Turn", outcome: "completed" }),
      finished("stage", "preparation", 0, 10, {
        parent_node_id: "turn",
        label: "Preparation",
      }),
    ]);

    expect(graph.nodes.map((node) => node.label)).toEqual(["Turn", "Preparation"]);
  });

  it("exports an offline report with escaped labels and explicit unknown usage", () => {
    const html = renderExplainAnalyzeHtml(
      [
        finished("attempt", "provider_attempt", 2, 20, {
          round_index: 0,
          attempt_index: 0,
          label: "<script>alert('x')</script>",
          usage: { basis: "provider_partial", fresh_input_tokens: 0 },
        }),
      ],
      { title: "Run <one>" },
    );

    expect(html).toContain("Run &lt;one&gt;");
    expect(html).toContain("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;");
    expect(html).not.toContain("<script>");
    expect(html).toContain("Unknown lanes stay unknown.");
    expect(html).toContain("<span>Fresh input</span><strong>0</strong>");
    expect(html).toContain("<span>Cache read</span><strong>Not fully reported</strong>");
    expect(html).not.toContain("https://");
  });

  it("shows elapsed time, retries, token totals, parentage, and parallel overlap", () => {
    const events = [
      started("turn", "turn", 0, { label: "Answer the customer" }),
      finished("turn", "turn", 0, 8_500, {
        label: "Answer the customer",
        outcome: "completed",
      }),
      started("round", "model_round", 650, {
        parent_node_id: "turn",
        round_index: 0,
        label: "Compose answer",
      }),
      finished("round", "model_round", 650, 8_000, {
        parent_node_id: "turn",
        round_index: 0,
        label: "Compose answer",
        outcome: "completed",
      }),
      started("request-0", "provider_attempt", 700, {
        parent_node_id: "round",
        round_index: 0,
        attempt_index: 0,
        label: "Model request",
      }),
      finished("request-0", "provider_attempt", 700, 4_750, {
        parent_node_id: "round",
        round_index: 0,
        attempt_index: 0,
        label: "Model request",
        outcome: "failed",
        usage: {
          basis: "provider_exact",
          fresh_input_tokens: 2_100,
          cache_read_tokens: 3_400,
          cache_creation_tokens: 0,
          output_tokens: 48,
        },
      }),
      started("retry-wait", "wait", 4_750, {
        parent_node_id: "round",
        label: "Retry delay",
      }),
      finished("retry-wait", "wait", 4_750, 5_000, {
        parent_node_id: "round",
        label: "Retry delay",
        outcome: "waiting",
      }),
      started("request-1", "provider_attempt", 5_000, {
        parent_node_id: "round",
        round_index: 0,
        attempt_index: 1,
        label: "Model request",
      }),
      finished("request-1", "provider_attempt", 5_000, 8_000, {
        parent_node_id: "round",
        round_index: 0,
        attempt_index: 1,
        label: "Model request",
        outcome: "succeeded",
        usage: {
          basis: "provider_exact",
          fresh_input_tokens: 2_100,
          cache_read_tokens: 3_400,
          cache_creation_tokens: 0,
          output_tokens: 146,
        },
      }),
      started("batch", "tool_batch", 8_050, {
        parent_node_id: "turn",
        label: "Load checkout data",
      }),
      finished("batch", "tool_batch", 8_050, 8_400, {
        parent_node_id: "turn",
        label: "Load checkout data",
        outcome: "completed",
      }),
      started("cart", "tool_call", 8_100, {
        parent_node_id: "batch",
        dependency_node_ids: ["request-1"],
        label: "Read customer cart",
      }),
      finished("cart", "tool_call", 8_100, 8_400, {
        parent_node_id: "batch",
        dependency_node_ids: ["request-1"],
        label: "Read customer cart",
        outcome: "succeeded",
      }),
      started("inventory", "tool_call", 8_120, {
        parent_node_id: "batch",
        dependency_node_ids: ["request-1"],
        label: "Check inventory",
      }),
      finished("inventory", "tool_call", 8_120, 8_320, {
        parent_node_id: "batch",
        dependency_node_ids: ["request-1"],
        label: "Check inventory",
        outcome: "succeeded",
      }),
    ];
    const html = renderExplainAnalyzeHtml(events, {
      title: "Checkout request Explain Analyze",
    });

    expect(html).toContain("<span class=\"metric-label\">Turn time</span><strong>8.5 s</strong>");
    expect(html).toContain("<strong>4.1 s</strong>");
    expect(html).toContain("<span class=\"metric-label\">Provider requests</span><strong>2</strong><small>1 failed or interrupted</small>");
    expect(html).toContain("<span>Fresh input</span><strong>4,200</strong>");
    expect(html).toContain("<span>Cache read</span><strong>6,800</strong>");
    expect(html).toContain("<span>Cache created</span><strong>0</strong>");
    expect(html).toContain("<span>Output</span><strong>194</strong>");
    expect(html).toContain("in 2,100 · cache 3,400 · write 0 · out 48");
    expect(html).toContain(
      'title="Fresh input: 2,100 · Cache read: 3,400 · Cache creation: 0 · Output: 48 (Provider reported)"',
    );
    expect(html).toContain("<strong>2 at once</strong>");
    expect(html).toContain("After “Model request (Round 1 · request 2)”");
    expect(html).toContain("<span class=\"tree-count\">2 stages</span>");
    expect(html).toContain("class=\"node-children\"");
    expect(html).toContain("Parent stage</span>");
    expect(html).toContain("Work stage</span>");
    expect(html).toContain('class="bar is-group status-complete"');
    expect(html).toContain(".node-children:before");
    expect(html).toContain("Totals include every request");
  });
});


describe("Explain Analyze integrity", () => {
  it("ignores unrelated events but exposes invalid explain facts in a completed report", () => {
    const turn = finished("turn", "turn", 0, 100);
    expect(reduceExplainAnalyzeEvents([turn, { type: "delta", text: "hello" }, null]).integrity).toBe("consistent");
    const malformed = { ...finished("tool", "tool_call", 10, 50), duration_ms: -1 };
    const graph = reduceExplainAnalyzeEvents([turn, malformed]);
    expect(graph.integrity).toBe("unknown");
    expect(graph.diagnostics).toContainEqual({ code: "invalid_event" });
    expect(explainAnalyzeMaxConcurrency(graph)).toBeNull();
    expect(renderExplainAnalyzeHtml([turn, malformed])).toContain('>Incomplete</span>');
  });

  it("repairs dangling edges when facts arrive without requiring a separate start", () => {
    const tool = finished("tool", "tool_call", 20, 50, {
      parent_node_id: "turn", dependency_node_ids: ["preparation"],
    });
    expect(reduceExplainAnalyzeEvents([tool]).diagnostics).toEqual([
      { code: "missing_dependency", nodeId: "tool", relatedNodeId: "preparation" },
      { code: "missing_parent", nodeId: "tool", relatedNodeId: "turn" },
    ]);
    const repaired = reduceExplainAnalyzeEvents([
      tool, finished("turn", "turn", 0, 100),
      finished("preparation", "preparation", 0, 20, { parent_node_id: "turn" }),
    ]);
    expect(repaired.integrity).toBe("consistent");
    expect(repaired.diagnostics).toEqual([]);
    expect(explainAnalyzeMaxConcurrency(repaired)).toBe(1);
  });

  it.each(["parent", "dependency"] as const)("reports %s cycles regardless of replay order", (edge) => {
    const a = finished("a", "preparation", 0, 10, edge === "parent"
      ? { parent_node_id: "b" } : { dependency_node_ids: ["b"] });
    const b = finished("b", "preparation", 0, 10, edge === "parent"
      ? { parent_node_id: "a" } : { dependency_node_ids: ["a"] });
    const graph = reduceExplainAnalyzeEvents([a, b]);
    expect(graph.integrity).toBe("unknown");
    expect(graph.diagnostics).toContainEqual({ code: `${edge}_cycle`, nodeId: "b", relatedNodeId: "a" });
    expect(reduceExplainAnalyzeEvents([b, a]).diagnostics).toEqual(graph.diagnostics);
    expect(explainAnalyzeMaxConcurrency(graph)).toBeNull();
  });

  it("marks both affected nodes when one event identity is reused", () => {
    const first = finished("a", "preparation", 0, 10);
    const graph = reduceExplainAnalyzeEvents([first, { ...first, node_id: "b" }]);
    expect(graph.integrity).toBe("unknown");
    expect(graph.nodes[0].conflicted).toBe(true);
    expect(graph.conflictedNodeIds).toEqual(["a", "b"]);
    expect(explainAnalyzeMaxConcurrency(graph)).toBeNull();
  });

  it("does not infer a final peak from active work and scopes unresolved nodes to their terminal turn", () => {
    const active = started("tool", "tool_call", 10);
    expect(explainAnalyzeMaxConcurrency(reduceExplainAnalyzeEvents([active]))).toBeNull();
    const graph = reduceExplainAnalyzeEvents([active, finished("turn", "turn", 0, 100)]);
    expect(graph.diagnostics).toContainEqual({ code: "unresolved_terminal_node", nodeId: "tool" });
    expect(reduceExplainAnalyzeEvents([
      active, finished("other-turn", "turn", 0, 100, { turn_id: "turn-2" }),
    ]).integrity).toBe("consistent");
  });
});

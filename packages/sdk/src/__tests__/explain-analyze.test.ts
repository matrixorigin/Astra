import { describe, expect, it } from "vitest";
import {
  explainAnalyzeFactFingerprint,
  explainAnalyzeMaxConcurrency,
  explainAnalyzeTurnOutcome,
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
  it("canonicalizes empty dependencies the same way for live and indexed replay facts", () => {
    const live = started("attempt", "provider_attempt", 0, {
      round_index: 0,
      attempt_index: 0,
    });
    const replay = { ...live, dependency_node_ids: [], index: 7 };

    expect(explainAnalyzeFactFingerprint(live)).toBe(explainAnalyzeFactFingerprint(replay));
    const graph = reduceExplainAnalyzeEvents([live, replay]);
    expect(graph.duplicateEventCount).toBe(1);
    expect(graph.conflictedNodeIds).toEqual([]);
  });

  it("surfaces measured coverage gaps without treating them as graph corruption", () => {
    const gaps: NonNullable<ExplainAnalyzeEventV1["coverage_gaps"]> = [
      "child_run_intervals",
      "tool_io_wait_intervals",
    ];
    const turn = finished("turn", "turn", 0, 100, {
      outcome: "completed",
      coverage_gaps: gaps,
    });
    const graph = reduceExplainAnalyzeEvents([turn]);

    expect(graph.integrity).toBe("consistent");
    expect(graph.coverageGaps).toEqual(gaps);
    expect(graph.nodes[0].coverageGaps).toEqual(gaps);
    expect(renderExplainAnalyzeHtml([turn])).toContain(
      "child-run timing · tool I/O wait breakdown",
    );
    expect(isExplainAnalyzeEventV1({
      ...started("turn", "turn", 0),
      coverage_gaps: gaps,
    })).toBe(false);
    expect(isExplainAnalyzeEventV1({
      ...finished("attempt", "provider_attempt", 0, 5, {
        round_index: 0,
        attempt_index: 0,
      }),
      coverage_gaps: gaps,
    })).toBe(false);
  });

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
    expect(html).toContain("Provider tokens · in 0 · cache read unknown");
    expect(html).toContain("cache write unknown · out unknown");
    expect(html).toContain("script-free snapshot");
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

    expect(html).toContain('<div class="report-result"><strong>8.5 s</strong>');
    expect(html).toContain("Provider tokens · in 4,200 · cache read 6,800 · cache write 0 · out 194");
    expect(html).toContain("in 2,100 · cache 3,400 · write 0 · out 48");
    expect(html).toContain(
      'title="Fresh input: 2,100 · Cache read: 3,400 · Cache creation: 0 · Output: 48 (Provider reported)"',
    );
    expect(html).toContain("Observed overlap · 2 overlapping recorded spans at peak");
    expect(html).toContain("after “Model request (Round 1 · request 2)”");
    expect(html).toContain("2 children");
    expect(html).toContain("class=\"node-children\"");
    expect(html).toContain('class="bar kind-turn is-group status-complete"');
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

describe("context facts", () => {
  const budget = { basis: "pre_provider_estimate" as const, estimated_input_tokens: 4200,
    estimated_system_tokens: 1400, tool_schema_tokens: 900, requested_output_tokens: 2000,
    reserved_protocol_tokens: 300, effective_input_limit_tokens: 12000,
    model_context_limit_tokens: 16000, visible_tool_count: 8 };
  const assembly = { basis: "runtime_text_estimate" as const,
    sources: [{ kind: "memory" as const, section_count: 2, estimated_tokens: 210 }] };

  it("preserves separately scoped request budgets and assembly estimates through replay/export", () => {
    const prepared = finished("prepared", "preparation", 0, 20, { context: { budget } });
    const assembled = finished("context", "context_assembly", 0, 10, { context: { assembly } });
    const graph = reduceExplainAnalyzeEvents([prepared, assembled, { ...prepared, index: 12 }]);
    expect(graph.duplicateEventCount).toBe(1);
    expect(graph.nodes.find((n) => n.nodeId === "prepared")?.context?.budget).toEqual(budget);
    expect(graph.nodes.find((n) => n.nodeId === "context")?.context?.assembly).toEqual(assembly);
    expect(graph.nodes.every((n) => n.usage === undefined)).toBe(true);
    const html = renderExplainAnalyzeHtml([prepared, assembled]);
    expect(html).toContain("4,200 tokens");
    expect(html).toContain("Retrieved memory");
    expect(html).toContain("Not billed usage");
  });

  it.each([
    { budget: { ...budget, raw_prompt: "private" } },
    { budget: { ...budget, estimated_input_tokens: Number.MAX_SAFE_INTEGER + 1 } },
    { budget: { ...budget, visible_tool_count: 0x1_0000_0000 } },
    { budget: { ...budget, basis: "provider_exact" } },
    {},
  ])("rejects malformed or content-bearing request metrics", (context) => {
    expect(isExplainAnalyzeEventV1({ ...finished("p", "preparation", 0, 10), context })).toBe(false);
  });

  it("rejects wrong stage scope, duplicate sources, previews and unsafe counts", () => {
    expect(isExplainAnalyzeEventV1({ ...started("p", "preparation", 0), context: { budget } })).toBe(false);
    expect(isExplainAnalyzeEventV1({ ...finished("p", "preparation", 0, 10), context: { budget }, usage: { basis: "provider_exact" } })).toBe(false);
    expect(isExplainAnalyzeEventV1({ ...finished("c", "context_assembly", 0, 10), context: { budget } })).toBe(false);
    expect(isExplainAnalyzeEventV1({ ...finished("p", "preparation", 0, 10), context: { assembly } })).toBe(false);
    for (const sources of [
      [assembly.sources[0], assembly.sources[0]],
      [{ ...assembly.sources[0], content_preview: "private memory" }],
      [{ ...assembly.sources[0], section_count: 0x1_0000_0000 }],
      [{ ...assembly.sources[0], kind: "raw_trace" }],
      [{ ...assembly.sources[0], kind: ["memory"] }],
      [{ ...assembly.sources[0], kind: { toString: null } }],
    ]) expect(isExplainAnalyzeEventV1({ ...finished("c", "context_assembly", 0, 10), context: { assembly: { ...assembly, sources } } })).toBe(false);
  });

  it("flags changed context on a repeated terminal as a conflicting fact", () => {
    const event = finished("p", "preparation", 0, 10, { context: { budget } });
    const graph = reduceExplainAnalyzeEvents([event, { ...event, event_id: "other", context: { budget: { ...budget, requested_output_tokens: 4000 } } }]);
    expect(graph.integrity).toBe("unknown");
    expect(graph.conflictedNodeIds).toContain("p");
  });
});

describe("saved Explain snapshots", () => {
  it("does not let a closed turn imply a missing end in another clock domain", () => {
    const html = renderExplainAnalyzeHtml([
      finished("closed", "turn", 0, 100),
      started("open", "turn", 0, { turn_id: "child-turn", clock_domain_id: "child-clock" }),
    ]);
    expect(html).not.toContain("Some execution facts are missing or conflict.");
    expect(html).toContain("Open at capture");
    expect(html).toContain("End not recorded");
    expect(html).toContain("script-free snapshot");
    expect(html).not.toContain(" – Now");
  });
  it("reports an unfinished child of a closed turn as incomplete", () => {
    const html = renderExplainAnalyzeHtml([
      finished("closed", "turn", 0, 100),
      started("tool", "tool_call", 10, { parent_node_id: "closed" }),
    ]);
    expect(html).toContain("Some execution facts are missing or conflict.");
    expect(html).toContain("End not recorded");
    expect(html).toContain('width:2px');
  });
  it.each([["preparation"], { toString: null }])("rejects non-string event enums without coercion", (kind) => {
    expect(isExplainAnalyzeEventV1({ ...finished("p", "preparation", 0, 10), kind })).toBe(false);
    expect(isExplainAnalyzeEventV1({ ...finished("p", "preparation", 0, 10), outcome: kind })).toBe(false);
    expect(isExplainAnalyzeEventV1({ ...finished("p", "preparation", 0, 10), usage: { basis: kind } })).toBe(false);
  });
});

it("aggregates independent turn outcomes without sorting clocks into execution order", () => {
  for (const clocks of [["a", "z"], ["z", "a"]]) {
    const events = [
      finished("one", "turn", 0, 10, { outcome: "failed", clock_domain_id: clocks[0] }),
      finished("two", "turn", 0, 20, { outcome: "succeeded", turn_id: "other", clock_domain_id: clocks[1] }),
    ];
    for (const input of [events, [...events].reverse()]) {
      expect(explainAnalyzeTurnOutcome(reduceExplainAnalyzeEvents(input).nodes)).toBe("Mixed outcomes");
      expect(renderExplainAnalyzeHtml(input)).toContain(">Mixed outcomes</span>");
    }
  }
});

it("counts started-only requests and labels known usage as a reported subtotal", () => {
  const html = renderExplainAnalyzeHtml([
    finished("turn", "turn", 0, 100),
    finished("first", "provider_attempt", 0, 20, { round_index: 0, attempt_index: 0,
      usage: { basis: "provider_exact", fresh_input_tokens: 40, output_tokens: 2 } }),
    started("second", "provider_attempt", 30, { round_index: 0, attempt_index: 1 }),
  ]);
  expect(html).toContain("Provider tokens · in 40 · cache read unknown · cache write unknown · out 2 · 1/2 requests reported");
});

it("retains reported lanes when another terminal request omits usage entirely", () => {
  const html = renderExplainAnalyzeHtml([
    finished("a", "provider_attempt", 0, 10, { round_index: 0, attempt_index: 0,
      usage: { basis: "provider_partial", fresh_input_tokens: 40, output_tokens: 2 } }),
    finished("b", "provider_attempt", 10, 20, { round_index: 0, attempt_index: 1 }),
  ]);
  expect(html).toContain("Provider tokens · in 40 · cache read unknown · cache write unknown · out 2 · 1/2 requests reported");
});

it("keeps known token lane subtotals when a reported request omits one lane", () => {
  const html = renderExplainAnalyzeHtml([
    finished("a", "provider_attempt", 0, 10, {
      round_index: 0,
      attempt_index: 0,
      usage: { basis: "provider_partial", fresh_input_tokens: 40, output_tokens: 2 },
    }),
    finished("b", "provider_attempt", 10, 20, {
      round_index: 0,
      attempt_index: 1,
      usage: { basis: "provider_partial", output_tokens: 3 },
    }),
  ]);
  expect(html).toContain("Provider tokens · in 40 (1/2) · cache read unknown · cache write unknown · out 5");
});

it("does not promise live updates in an empty or started-only exported snapshot", () => {
  for (const events of [[], [started("open", "turn", 0)]]) {
    const html = renderExplainAnalyzeHtml(events);
    expect(html).not.toContain("In progress");
    expect(html).not.toContain("as the run advances");
    expect(html).toContain("script-free snapshot");
  }
});

it("does not count context assembly inside request preparation as parallel work", () => {
  const graph = reduceExplainAnalyzeEvents([
    finished("turn", "turn", 0, 100),
    finished("preparation", "preparation", 0, 90, { parent_node_id: "turn" }),
    finished("assembly", "context_assembly", 5, 80, { parent_node_id: "preparation" }),
  ]);
  expect(graph.integrity).toBe("consistent");
  expect(explainAnalyzeMaxConcurrency(graph)).toBe(1);
});

it("shows approval wait intervals without counting them as parallel work", () => {
  const events = [
    finished("turn", "turn", 0, 180, {
      coverage_gaps: ["tool_io_wait_intervals"],
    }),
    finished("batch", "tool_batch", 0, 180, { parent_node_id: "turn" }),
    finished("admission-a", "admission", 0, 70, { parent_node_id: "batch" }),
    finished("approval-a", "wait", 10, 70, {
      label: "Waiting for approval to run command A",
      parent_node_id: "admission-a",
    }),
    finished("tool-a", "tool_call", 70, 120, { parent_node_id: "batch" }),
    finished("admission-b", "admission", 0, 120, { parent_node_id: "batch" }),
    finished("approval-b", "wait", 80, 120, {
      label: "Waiting for approval to run command B",
      parent_node_id: "admission-b",
    }),
    finished("tool-b", "tool_call", 120, 170, { parent_node_id: "batch" }),
  ];
  const graph = reduceExplainAnalyzeEvents(events);
  const html = renderExplainAnalyzeHtml(events);

  expect(graph.integrity).toBe("consistent");
  expect(explainAnalyzeMaxConcurrency(graph)).toBe(1);
  expect(html).toContain("Measured wait time");
  expect(html).toContain("100 ms");
  expect(html).toContain("kind-wait");
  expect(html).toContain("kind-admission");
  expect(html).toContain("tool I/O wait breakdown");
});

it("exports graph nodes with only recorded relationships and retains tree and timeline views", () => {
  const html = renderExplainAnalyzeHtml([
    finished("turn", "turn", 0, 100),
    finished("a", "tool_call", 10, 30, { parent_node_id: "turn", label: "Read file" }),
    finished("b", "tool_call", 40, 80, { parent_node_id: "turn", dependency_node_ids: ["a"], label: "Use file" }),
  ]);
  expect(html).toContain('id="secondary-graph"');
  expect(html).toContain('id="secondary-timeline"');
  expect(html).toContain('id="tree-view"');
  expect(html.match(/class="dag-edge dag-edge-parent"/g)).toHaveLength(2);
  expect(html.match(/class="dag-edge dag-edge-dependency"/g)).toHaveLength(1);
  expect(html).toContain('href="#report-stage-');
  expect(html).toContain('class="dag-inspection"');
  expect(html).not.toContain('<script>');
});

it("discloses graph layout limits while retaining all stages in the tree", () => {
  const html = renderExplainAnalyzeHtml(Array.from({ length: 501 }, (_, index) =>
    finished(`tool-${index}`, "tool_call", index * 2, index * 2 + 1)));
  expect(html).toContain("Showing 500 of 501 stages");
  expect(html).toContain('class="node-title" title="tool-500"');
});

it("keeps external delivery gaps inside the HTML report's copyable text", () => {
  const html = renderExplainAnalyzeHtml([], { degraded: true });
  const copyable = html.match(/<textarea[^>]*>([\s\S]*?)<\/textarea>/)?.[1];
  expect(copyable).toContain("Incomplete observation: delivery gap");
  expect(copyable).toContain("No execution facts recorded");
});

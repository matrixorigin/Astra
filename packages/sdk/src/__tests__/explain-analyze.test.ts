import { describe, expect, it } from "vitest";
import {
  explainAnalyzeFactFingerprint,
  explainAnalyzeAuxiliaryDetailsLines,
  explainAnalyzeAuxiliaryUsageLines,
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
  it("keeps bounded delegation-catalog evidence through admission replay and renderers", () => {
    const detail = {
      kind: "delegation_catalog_resolution" as const,
      requirement_index: 0,
      match_count: 3,
    };
    const terminal = finished("admission-agent", "admission", 4, 9, {
      outcome: "blocked",
      decision_detail: detail,
    });
    expect(isExplainAnalyzeEventV1(terminal)).toBe(true);
    expect(isExplainAnalyzeEventV1({ ...terminal, kind: "tool_call" })).toBe(false);
    expect(isExplainAnalyzeEventV1({ ...terminal, decision_detail: { ...detail, match_count: 1 } })).toBe(false);
    expect(isExplainAnalyzeEventV1({
      ...started("admission-agent", "admission", 4),
      decision_detail: detail,
    })).toBe(false);

    const duplicate = { ...terminal, event_id: "admission-agent:replay" };
    const graph = reduceExplainAnalyzeEvents([terminal, duplicate]);
    expect(graph.integrity).toBe("consistent");
    expect(graph.nodes[0].decisionDetail).toEqual(detail);
    expect(renderExplainAnalyzeHtml([terminal])).toContain(
      "3 active authorized Chat-capable catalog entries matched exactly",
    );

    const conflict = reduceExplainAnalyzeEvents([
      terminal,
      { ...duplicate, decision_detail: { ...detail, match_count: 2 } },
    ]);
    expect(conflict.conflictedNodeIds).toEqual(["admission-agent"]);
  });

  it("accepts and renders a terminal judgment stage", () => {
    const judgment = finished("tool-result-judgment", "judgment", 10, 25, {
      round_index: 0,
      label: "Tool-result judgment · jev-1.13.0 · selected 1/2 chunks",
    });

    expect(isExplainAnalyzeEventV1(judgment)).toBe(true);
    const graph = reduceExplainAnalyzeEvents([judgment]);
    expect(graph.integrity).toBe("consistent");
    expect(graph.nodes).toHaveLength(1);
    expect(graph.nodes[0]).toMatchObject({
      kind: "judgment",
      terminalObserved: true,
    });
    expect(renderExplainAnalyzeHtml([judgment])).toContain("jev-1.13.0");
  });

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

  it("preserves memory decisions in replay and renders their limits without private text", () => {
    const report = { session_id: "s", turn: 1, operation: "relevance" as const,
      method: "model" as const, reason: "completed" as const, model: "jev-test", elapsed_ms: 398, selection_order: [0],
      candidates: [{ index: 0, selected: true, probability_bps: 9000 }, { index: 1, selected: false, probability_bps: 1000 }] };
    const event = finished("context", "context_assembly", 0, 10, { context: { assembly: { ...assembly, edge_memory_selection: [report] } } });
    expect(isExplainAnalyzeEventV1(event)).toBe(true);
    const graph = reduceExplainAnalyzeEvents([event, event]);
    expect(graph.nodes[0].context?.assembly?.edge_memory_selection).toEqual([report]);
    const html = renderExplainAnalyzeHtml([event]);
    expect(html).toContain("2 candidates → 1 selected");
    expect(html).toContain("90.00%");
    expect(html).toContain("final request projection unavailable");
    for (const bad of [
      { ...report, reason: "no_candidates" }, { ...report, method: "lexical" },
      { ...report, session_id: "" }, { ...report, turn: 0 },
      { ...report, candidates: [{ index: 0, selected: true, probability_bps: 10001 }] },
      { ...report, candidates: [{ index: 1, selected: true, probability_bps: 9000 }] },
      { ...report, raw_response: "private memory" },
      { ...report, selection_order: [1] }, { ...report, selection_order: [0, 0] },
    ]) {
      expect(isExplainAnalyzeEventV1({ ...event, context: { assembly: { ...assembly, edge_memory_selection: [bad] } } })).toBe(false);
    }
  });

  it("preserves bounded memory coverage and distinguishes known from unknown prompt inclusion", () => {
    const base = { session_id: "s", turn: 1, operation: "relevance" as const,
      method: "model" as const, reason: "completed" as const, model: "jev-test", elapsed_ms: 398, selection_order: [0],
      candidates: [{ index: 0, selected: true, probability_bps: 9000 }],
      candidate_coverage: { source_items: 20, evaluated_candidates: 1, truncated: true } };
    for (const [included_candidates, expected] of [[1, "1/1 entered request"], [null, "request inclusion unknown"]] as const) {
      const report = { ...base, prompt_projection: { selected_candidates: 1, included_candidates } };
      const event = finished("context", "context_assembly", 0, 10, { context: { assembly: { ...assembly, edge_memory_selection: [report] } } });
      expect(isExplainAnalyzeEventV1(event)).toBe(true);
      expect(reduceExplainAnalyzeEvents([event]).nodes[0].context?.assembly?.edge_memory_selection).toEqual([report]);
      const html = renderExplainAnalyzeHtml([event]);
      expect(html).toContain("bounded 20 source items to 1 candidates");
      expect(html).toContain(expected);
      expect(html).toContain("final request projection measured");
    }
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


describe("auxiliary provider usage", () => {
  const auxiliary = {
    available: true,
    attempts: [{attempt_id: "aux-1", provider: "typesafe", offering_id: "jev-1", model_name: "jev1", purpose: "memory_retrieval_rerank", operation_id: "relevance", usage_status: "provider_partial" as const, usage: {basis: "provider_partial" as const, fresh_input_tokens: 42}}],
  };
  it("keeps overflowing captures visible with explicit lower-bound totals", () => {
    const event = finished("turn", "turn", 0, 100, {auxiliary_usage: {...auxiliary, truncated: true}});
    expect(isExplainAnalyzeEventV1(event)).toBe(true);
    const lines = explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([event]));
    expect(lines[0]).toContain("in at least 42");
    expect(lines[1]).toContain("capture truncated");
    expect(lines[1]).toContain("counts cover captured attempts only");
    expect(renderExplainAnalyzeHtml([event])).toContain("in at least 42");
    expect(isExplainAnalyzeEventV1(finished("bad", "turn", 0, 100, {
      auxiliary_usage: {available: false, truncated: true, attempts: []},
    }))).toBe(false);
    const empty = finished("empty", "turn", 0, 100, {
      auxiliary_usage: {available: true, truncated: true, attempts: []},
    });
    expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([empty]))).toEqual([
      "Auxiliary tokens · capture truncated; request counts cover captured attempts only; token sums are lower bounds",
    ]);
  });
  it("treats token lanes as lower bounds when a captured peer or segment has unknown usage", () => {
    const event = finished("turn", "turn", 0, 100, {auxiliary_usage: {
      ...auxiliary, attempts: [...auxiliary.attempts, {...auxiliary.attempts[0], attempt_id: "aux-2", usage_status: "unavailable", usage: undefined}],
    }});
    expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([event]))[0]).toContain("in at least 42");
    const missing = finished("missing", "turn", 100, 200, {auxiliary_usage: {available: false, attempts: []}});
    const known = finished("known", "turn", 0, 100, {auxiliary_usage: auxiliary});
    expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([known, missing]))[0]).toContain("in at least 42");
  });
  it("exports Jev separately, deduplicates physical attempts across segments, and preserves unknown lanes", () => {
    const first = finished("turn", "turn", 0, 100, {auxiliary_usage: auxiliary});
    const second = finished("segment", "turn", 100, 200, {auxiliary_usage: auxiliary});
    expect(isExplainAnalyzeEventV1(first)).toBe(true);
    const graph = reduceExplainAnalyzeEvents([first, second]);
    const lines = explainAnalyzeAuxiliaryUsageLines(graph);
    expect(lines).toHaveLength(1);
    expect(lines[0]).toContain("Jev");
    expect(lines[0]).toContain("in 42");
    expect(lines[0]).toContain("out unknown");
    expect(lines[0]).toContain("1/1 requests reported · partial");
    expect(renderExplainAnalyzeHtml([first, second])).toContain("Auxiliary tokens");
  });
  it("keeps same-model request classification, skill selection, and Work planning usage separate", () => {
    const operations = [
      ["request_judgment", "Request classification", 10],
      ["skill_auto_route", "Skill selection", 20],
      ["work_plan", "Work planning", 30],
      ["custom_judgment", "Request analysis", 40],
    ] as const;
    const event = finished("turn", "turn", 0, 100, {auxiliary_usage: {
      available: true,
      attempts: operations.map(([operation, , tokens]) => ({
        attempt_id: `aux-${operation}`, provider: "openai", offering_id: "same-offering", model_name: "same-model",
        purpose: "introspection", operation_id: operation, usage_status: "provider_exact" as const,
        usage: {basis: "provider_exact" as const, fresh_input_tokens: tokens, output_tokens: 1},
      })),
    }});
    expect(isExplainAnalyzeEventV1(event)).toBe(true);
    const lines = explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([event]));
    expect(lines).toHaveLength(4);
    const html = renderExplainAnalyzeHtml([event]);
    for (const [operation, label, tokens] of operations) {
      const line = lines.find(line => line.includes(label));
      expect(line).toContain(`operation ${operation} · offering same-offering`);
      expect(line).toContain(`in ${tokens} ·`);
      expect(line).toContain("1/1 requests reported");
      expect(line).toContain("cache read unknown");
      expect(html).toContain(label);
    }
  });
  it("isolates Jev and LLM counters and deduplicates repeated capture segments in text and HTML", () => {
    const usage = {
      available: true,
      attempts: [
        {attempt_id:"jev-decision", provider:"typesafe", offering_id:"jev-offering", model_name:"jev-model", purpose:"introspection", operation_id:"request_judgment", usage_status:"provider_exact" as const, usage:{basis:"provider_exact" as const, fresh_input_tokens:100, output_tokens:3}},
        {attempt_id:"llm-decision", provider:"openai", offering_id:"llm-offering", model_name:"llm-model", purpose:"introspection", operation_id:"request_judgment", usage_status:"provider_exact" as const, usage:{basis:"provider_exact" as const, fresh_input_tokens:40, cache_read_tokens:60, cache_creation_tokens:0, output_tokens:5}},
        {attempt_id:"llm-plan", provider:"openai", offering_id:"llm-offering", model_name:"llm-model", purpose:"introspection", operation_id:"work_plan", usage_status:"provider_exact" as const, usage:{basis:"provider_exact" as const, fresh_input_tokens:200, cache_read_tokens:10, cache_creation_tokens:7, output_tokens:20}},
      ],
    };
    const events = [
      finished("first", "turn", 0, 100, {auxiliary_usage:usage}),
      finished("second", "turn", 100, 200, {auxiliary_usage:usage}),
    ];
    expect(events.every(isExplainAnalyzeEventV1)).toBe(true);
    const lines = explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents(events));
    expect(lines).toHaveLength(3);
    const html = renderExplainAnalyzeHtml(events);
    for (const [identity, label, counts] of [
      ["Jev (jev-model)", "Request classification", "in 100 · cache read unknown · cache write unknown · out 3"],
      ["openai (llm-model)", "Request classification", "in 40 · cache read 60 · cache write 0 · out 5"],
      ["openai (llm-model)", "Work planning", "in 200 · cache read 10 · cache write 7 · out 20"],
    ]) {
      const line = lines.find(line => line.includes(identity) && line.includes(label));
      expect(line).toContain(counts);
      expect(line).toContain("1/1 requests reported");
      expect(line).not.toContain("partial");
      expect(html).toContain(line);
    }
  });
  it("uses purpose for completion proxy and a neutral label for unknown operations", () => {
    for (const [operation, purpose, label] of [
      ["completion_proxy:verification_judge", "verification_judge", "Verification"],
      ["completion_proxy:introspection", "introspection", "Request analysis"],
      ["unrecognized", "introspection", "Request analysis"],
      ["__proto__", "introspection", "Request analysis"],
      ["unrecognized", "unrecognized", "Auxiliary inference"],
    ]) {
      const event = finished("turn", "turn", 0, 100, {auxiliary_usage: {...auxiliary, attempts: [{...auxiliary.attempts[0], operation_id: operation, purpose}]}});
      const line = explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([event]))[0];
      expect(line).toContain(label);
      expect(line).not.toContain("Request decisions");
    }
  });
  it("rejects usage facts on nonterminal events and duplicated physical identities", () => {
    expect(isExplainAnalyzeEventV1(started("turn", "turn", 0, {auxiliary_usage: auxiliary}))).toBe(false);
    expect(isExplainAnalyzeEventV1(finished("turn", "turn", 0, 100, {auxiliary_usage: {...auxiliary, attempts: [auxiliary.attempts[0], auxiliary.attempts[0]]}}))).toBe(false);
  });
  it("keeps unavailable and all-zero partial usage distinct from known zero", () => {
    const partial = finished("turn", "turn", 0, 100, {auxiliary_usage: {...auxiliary, attempts: [{...auxiliary.attempts[0], usage: undefined}]}});
    expect(isExplainAnalyzeEventV1(partial)).toBe(true);
    expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([partial]))[0]).toContain("usage unavailable");
    const unavailable = finished("turn", "turn", 0, 100, {auxiliary_usage: {available: false, attempts: []}});
    expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([unavailable]))).toEqual(["Auxiliary tokens · capture unavailable"]);
  });
});

describe("auxiliary execution details", () => {
  it("accepts the runtime terminal payload and carries it through SDK projections", () => {
    const classifier = {
      result: "decided" as const,
      classification: {
        work_required: true,
        activation_deferred: false,
        domain: null,
        mutation: "read_only" as const,
        scope: "unknown" as const,
        parallel_subruns: false,
        capabilities: [] as const,
      },
    };
    const reconciled = {
      ...classifier,
      classification: { ...classifier.classification, activation_deferred: true },
    };
    const event = finished("turn", "turn", 0, 100, {
      auxiliary_details: {
        calls: [{
          call_id: "request/初期:0",
          operation_id: "request_judgment",
          stage: "initial",
          start_elapsed_ms: 12,
          duration_ms: 8,
          outcome: "succeeded",
        }],
        admission: {
          status: "accepted",
          reason: { kind: "accepted" },
          classification: classifier,
          decision: reconciled,
        },
      },
    });

    expect(isExplainAnalyzeEventV1(event)).toBe(true);
    const graph = reduceExplainAnalyzeEvents([event]);
    expect(graph.integrity).toBe("consistent");
    expect(graph.nodes[0].auxiliaryDetails?.admission?.decision).toEqual(reconciled);
    expect(explainAnalyzeAuxiliaryDetailsLines(graph)).toEqual([
      "Auxiliary scope · turn",
      "Auxiliary timing · request_judgment · call request/初期:0 · operation request_judgment · stage initial · 8 ms · client outcome succeeded · starts +12 ms · logical client interval; overlapping intervals are not added",
      expect.stringContaining("Admission settlement · status accepted"),
    ]);
    expect(renderExplainAnalyzeHtml([event])).toContain("Auxiliary execution details");
    expect(isExplainAnalyzeEventV1({
      ...event,
      event_id: "turn:no-call-interval",
      auxiliary_details: { admission: event.auxiliary_details!.admission },
    })).toBe(true);
  });

  it("rejects a terminal detail with unknown nested fields", () => {
    const event = finished("turn", "turn", 0, 100, {
      auxiliary_details: {
        calls: [],
        admission: {
          status: "accepted",
          reason: { kind: "accepted", extra: true } as never,
          decision: {
            result: "decided",
            classification: {
              work_required: true,
              activation_deferred: false,
              domain: null,
              mutation: "read_only",
              scope: "unknown",
              parallel_subruns: false,
              capabilities: [],
            },
          },
        },
      },
    });
    expect(isExplainAnalyzeEventV1(event)).toBe(false);

    const accepted = {
      ...event.auxiliary_details!.admission!,
      reason: { kind: "accepted" as const },
    };
    const invalidProvenance = {
      result: "abstained",
      uncertain_fields: ["required"],
      assessment: {
        provenance: ["discrete_decision"],
        fields: [{ field: "required", score: 0.5 }],
      },
    } as never;
    expect(isExplainAnalyzeEventV1({
      ...event,
      event_id: "turn:invalid-provenance-type",
      auxiliary_details: { admission: { ...accepted, classification: invalidProvenance } },
    })).toBe(false);

    const invalidDiscreteScore = {
      ...invalidProvenance,
      assessment: {
        provenance: "discrete_decision",
        fields: [{ field: "required", score: 0.2 }],
      },
    } as never;
    expect(isExplainAnalyzeEventV1({
      ...event,
      event_id: "turn:invalid-discrete-score",
      auxiliary_details: { admission: { ...accepted, classification: invalidDiscreteScore } },
    })).toBe(false);
  });
});

describe("auxiliary conflict parity", () => {
  const attempt = {attempt_id: "same", provider: "typesafe", offering_id: "jev", model_name: "jev", purpose: "introspection", operation_id: "request_judgment", usage_status: "provider_exact" as const, usage: {basis: "provider_exact" as const, fresh_input_tokens: 100}};
  type Attempt = NonNullable<ExplainAnalyzeEventV1["auxiliary_usage"]>["attempts"][number];
  const event = (id: string, a: Attempt = attempt) => finished(id, "turn", 0, 10, {auxiliary_usage: {available: true, attempts: [a]}});
  for (const field of ["provider", "offering_id", "model_name", "purpose", "operation_id", "fresh_input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens"]) {
    it(`rejects conflicting ${field} in either segment order`, () => {
      const bucket = field.endsWith("tokens");
      const first = bucket ? {...attempt, usage: {...attempt.usage, [field]: 100}} : attempt;
      const second = bucket ? {...attempt, usage: {...attempt.usage, [field]: 200}} : {...attempt, [field]: "different"};
      const events = [event("one", first), event("two", second)];
      for (const order of [events, [...events].reverse()]) {
        const graph = reduceExplainAnalyzeEvents(order);
        const lines = explainAnalyzeAuxiliaryUsageLines(graph);
        expect(lines).toEqual([expect.stringContaining("capture unavailable")]);
        expect(lines.join()).not.toContain("requests reported");
        const html = renderExplainAnalyzeHtml(order);
        expect(html).toContain("totals unavailable");
        expect(html).not.toContain("1/1 requests reported");
      }
    });
  }
  it("retains same-node and reused-event conflicts even when incoming usage is discarded", () => {
    for (const sameEventId of [false, true]) {
      const without = finished("one", "preparation", 0, 10);
      const withUsage = {...event("one"), event_id: sameEventId ? without.event_id : "other-event"};
      const replay = event("independent");
      for (const order of [[without, withUsage, replay], [withUsage, without, replay], [replay, without, withUsage]]) {
        expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents(order))).toEqual([expect.stringContaining("capture unavailable")]);
        expect(renderExplainAnalyzeHtml(order)).toContain("totals unavailable");
      }
      for (const order of [[without, withUsage], [withUsage, without]]) {
        const html = renderExplainAnalyzeHtml(order);
        expect(html).toContain("capture unavailable");
        expect(html).not.toContain("requests reported");
      }
      const sameTurn = finished("one", "turn", 0, 10);
      for (const order of [[sameTurn, withUsage, replay], [withUsage, sameTurn, replay]]) {
        expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents(order))).toEqual([expect.stringContaining("capture unavailable")]);
      }
    }
  });
  it("keeps weaker known buckets as conflict evidence without merging them into exact usage", () => {
    const partial = event("partial", {...attempt, usage_status: "provider_partial", usage: {basis: "provider_partial", fresh_input_tokens: 100, output_tokens: 3}});
    const exact = event("exact");
    const conflicting = event("conflicting", {...attempt, usage: {...attempt.usage, output_tokens: 4}});
    for (const order of [[partial, exact], [exact, partial]]) {
      expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents(order))[0]).toContain("out unknown");
      expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([...order, conflicting]))[0]).toContain("capture unavailable");
    }
  });
});

it("upgrades auxiliary usage from unavailable through partial to exact across segments", () => {
  const attempt = {attempt_id: "aux-1", provider: "typesafe", offering_id: "jev-1", model_name: "jev1", purpose: "verification_judge", operation_id: "verification_judge"};
  const missing = finished("one", "turn", 0, 10, {auxiliary_usage: {available: true, attempts: [{...attempt, usage_status: "unavailable"}]}});
  const partial = finished("two", "turn", 10, 20, {auxiliary_usage: {available: true, attempts: [{...attempt, usage_status: "provider_partial"}]}});
  expect(explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents([missing, partial]))[0]).toContain("partial");
  const exact = finished("three", "turn", 20, 30, {auxiliary_usage: {available: true, attempts: [{...attempt, usage_status: "provider_exact", usage: {basis: "provider_exact", fresh_input_tokens: 100, output_tokens: 0}}]}});
  for (const order of [[missing, partial, exact], [exact, partial, missing]]) {
    const output = explainAnalyzeAuxiliaryUsageLines(reduceExplainAnalyzeEvents(order))[0];
    expect(output).toContain("in 100");
    expect(output).toContain("out 0");
    expect(output).toContain("cache read unknown");
    expect(output).not.toContain("partial");
  }
});

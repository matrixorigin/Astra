import { describe, expect, it } from "vitest";
import { renderExplainAnalyzeText } from "../index";
import type { ExplainAnalyzeEventV1 } from "../types";

const identity = {
  type: "explain_analyze" as const,
  schema_version: 1 as const,
  run_id: "run-1",
  turn_id: "turn-1",
  producer_id: "producer-1",
  clock_domain_id: "clock-a",
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

describe("Explain Analyze text export", () => {
  it("renders recorded parent branches, durations, outcomes, domains, and dependencies", () => {
    const events = [
      started("turn", "turn", 0, { label: "Customer request" }),
      finished("turn", "turn", 0, 100, {
        label: "Customer request",
        outcome: "completed",
      }),
      started("batch", "tool_batch", 20, {
        parent_node_id: "turn",
        label: "Load data",
      }),
      finished("batch", "tool_batch", 20, 80, {
        parent_node_id: "turn",
        label: "Load data",
        outcome: "completed",
      }),
      started("cart", "tool_call", 25, {
        parent_node_id: "batch",
        label: "Read cart",
      }),
      finished("cart", "tool_call", 25, 40, {
        parent_node_id: "batch",
        label: "Read cart",
      }),
      started("inventory", "tool_call", 45, {
        parent_node_id: "batch",
        dependency_node_ids: ["cart"],
        label: "Read inventory",
      }),
      finished("inventory", "tool_call", 45, 70, {
        parent_node_id: "batch",
        dependency_node_ids: ["cart"],
        label: "Read inventory",
      }),
      finished("edge-turn", "turn", 0, 12, {
        clock_domain_id: "clock-b",
        label: "Edge request",
        outcome: "completed",
      }),
    ];

    const text = renderExplainAnalyzeText(events);

    expect(text).toContain("## Clock domain: clock-a");
    expect(text).toContain("## Clock domain: clock-b");
    expect(text).toContain(
      "Customer request · 100 ms · Completed",
    );
    expect(text).toContain(
      "└─ Load data · 60 ms · Completed",
    );
    expect(text).toContain(
      "   ├─ Read cart · 15 ms · Succeeded",
    );
    expect(text).toContain('depends on (recorded): "Read cart"');
    expect(text).toContain("Edge request · 12 ms · Completed");
    expect(text).not.toContain("cart:finish");
  });

  it("keeps unavailable token lanes unknown and prints context estimates in tokens", () => {
    const text = renderExplainAnalyzeText([
      finished("attempt", "provider_attempt", 10, 30, {
        round_index: 0,
        attempt_index: 0,
        label: "Provider request",
        usage: {
          basis: "provider_partial",
          fresh_input_tokens: 0,
          output_tokens: 2,
        },
      }),
      finished("prepared", "preparation", 0, 5, {
        label: "Request budget",
        context: {
          budget: {
            basis: "pre_provider_estimate",
            estimated_input_tokens: 4_200,
            estimated_system_tokens: 1_400,
            tool_schema_tokens: 900,
            requested_output_tokens: 2_000,
            reserved_protocol_tokens: 300,
            effective_input_limit_tokens: 12_000,
            model_context_limit_tokens: 16_000,
            visible_tool_count: 3,
          },
        },
      }),
      finished("assembled", "context_assembly", 0, 4, {
        label: "Context assembly",
        context: {
          assembly: {
            basis: "runtime_text_estimate",
            sources: [{ kind: "memory", section_count: 2, estimated_tokens: 210 }],
          },
        },
      }),
      started("open", "wait", 31, { label: "Pending wait" }),
    ]);

    expect(text).toContain("fresh input: 0 tokens");
    expect(text).toContain("cache read: unknown");
    expect(text).toContain("cache creation: unknown");
    expect(text).toContain("output: 2 tokens");
    expect(text).not.toContain("cache read: 0 tokens");
    expect(text).toContain("estimated input: 4,200 tokens");
    expect(text).toContain("model context limit: 16,000 tokens");
    expect(text).toContain("visible tools: 3");
    expect(text).toContain("Retrieved memory: 210 tokens (2 sections)");
    expect(text).toContain(
      "Pending wait · unknown · unknown",
    );
  });

  it("terminates on parent cycles and bounds deep traversal without recursive calls", () => {
    const cycle = [
      finished("cycle-a", "preparation", 0, 1, {
        parent_node_id: "cycle-b",
        label: "Cycle A",
      }),
      finished("cycle-b", "preparation", 1, 2, {
        parent_node_id: "cycle-a",
        label: "Cycle B",
      }),
    ];
    const deep = Array.from({ length: 160 }, (_, index) =>
      finished(`deep-${index}`, "preparation", index, index + 1, {
        parent_node_id: index === 0 ? undefined : `deep-${index - 1}`,
        label: `Depth ${index}`,
      }),
    );

    const text = renderExplainAnalyzeText([...cycle, ...deep]);

    expect(text).toContain("Cycle A");
    expect(text).toContain("Cycle B");
    expect(text).toContain("Depth 0");
    expect(text).toContain("nested stages omitted at depth limit");
    expect(text.length).toBeLessThan(256_100);
  });
});

it("preserves external delivery gaps independently of structural consistency", () => {
  const events = [finished("turn", "turn", 0, 100, { outcome: "completed" })];
  const text = renderExplainAnalyzeText(events, { degraded: true });
  expect(text).toContain("Incomplete observation: delivery gap");
  expect(text).toContain("Structural integrity: consistent");
  expect(text).toContain("turn · 100 ms · Completed");
  expect(renderExplainAnalyzeText(events)).not.toContain("Incomplete observation");
});

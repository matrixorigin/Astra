import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { vi } from "vitest";
import { ExplainAnalyzePanel } from "@/components/app/explain-analyze-panel";

function renderTimeline(element: Parameters<typeof render>[0]) {
  const view = render(element);
  fireEvent.click(screen.getByRole("button", { name: "Timeline" }));
  return view;
}

const identity = {
  type: "explain_analyze" as const,
  schema_version: 1 as const,
  run_id: "run-1",
  turn_id: "turn-1",
  producer_id: "worker-1",
  clock_domain_id: "clock-1",
};

describe("ExplainAnalyzePanel", () => {
  it("shows a plain-language overview, honest token lanes, and an expandable timeline", () => {
    renderTimeline(
      <ExplainAnalyzePanel
        events={[
          {
            ...identity,
            event_id: "turn:start",
            node_id: "turn",
            kind: "turn",
            label: "User turn",
            transition: "started",
            elapsed_ms: 0,
          },
          {
            ...identity,
            event_id: "turn:finish",
            node_id: "turn",
            kind: "turn",
            label: "User turn",
            transition: "finished",
            elapsed_ms: 2_000,
            start_elapsed_ms: 0,
            duration_ms: 2_000,
            outcome: "completed",
          },
          {
            ...identity,
            event_id: "round:start",
            node_id: "round",
            parent_node_id: "turn",
            kind: "model_round",
            round_index: 0,
            label: "Draft answer",
            transition: "started",
            elapsed_ms: 200,
          },
          {
            ...identity,
            event_id: "round:finish",
            node_id: "round",
            parent_node_id: "turn",
            kind: "model_round",
            round_index: 0,
            label: "Draft answer",
            transition: "finished",
            elapsed_ms: 1_400,
            start_elapsed_ms: 200,
            duration_ms: 1_200,
            outcome: "completed",
          },
          {
            ...identity,
            event_id: "attempt:start",
            node_id: "attempt",
            parent_node_id: "round",
            kind: "provider_attempt",
            round_index: 0,
            attempt_index: 0,
            label: "Model request",
            transition: "started",
            elapsed_ms: 250,
          },
          {
            ...identity,
            event_id: "attempt:finish",
            node_id: "attempt",
            parent_node_id: "round",
            kind: "provider_attempt",
            round_index: 0,
            attempt_index: 0,
            label: "Model request",
            transition: "finished",
            elapsed_ms: 1_250,
            start_elapsed_ms: 250,
            duration_ms: 1_000,
            outcome: "succeeded",
            usage: {
              basis: "provider_partial",
              fresh_input_tokens: 18,
              cache_read_tokens: 0,
              output_tokens: 5,
            },
          },
        ]}
      />,
    );

    expect(screen.getByText("Timings, dependencies and model usage")).toBeTruthy();
    expect(screen.getByText("Turn time")).toBeTruthy();
    expect(screen.getByTitle("2.0 s")).toBeTruthy();
    expect(screen.getByText("Slowest model request")).toBeTruthy();
    expect(screen.getByTitle("1.0 s")).toBeTruthy();
    expect(screen.getByText("18")).toBeTruthy();
    expect(screen.getByText("Token usage partly reported")).toBeTruthy();
    expect(screen.getByLabelText("Graph legend")).toBeTruthy();
    expect(screen.getByText("Parent stage")).toBeTruthy();
    expect(screen.getByText("Work stage")).toBeTruthy();
    const parentTimeline = screen.getByRole("button", {
      name: /User turn, parent stage with 1 nested stage/,
    });
    expect(parentTimeline.classList.contains("explain-analyze-parent-bar")).toBeTruthy();

    const graphToggle = screen.getByRole("button", { name: /Execution graph/ });
    expect(graphToggle.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(graphToggle);
    expect(graphToggle.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(graphToggle);
    expect(screen.getByRole("button", { name: /Model request/ })).toBeTruthy();
    expect(screen.getByText(/in 18 · cache 0 · out 5 · partial/)).toBeTruthy();
    const turnToggle = screen.getByRole("button", { name: "Collapse User turn" });
    fireEvent.click(turnToggle);
    expect(screen.queryByText("Draft answer")).toBeNull();
    expect(screen.queryByRole("button", { name: /Inspect Model request/ })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Expand User turn" }));
    expect(screen.getByText("Draft answer")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Model request/ })).toBeTruthy();
  });

  it("makes delivery gaps visible instead of presenting a complete graph", () => {
    renderTimeline(<ExplainAnalyzePanel events={[]} degraded />);
    expect(screen.getByText(/delivery gap or unresolved stages/i)).toBeTruthy();
  });

  it("shows a warning when every Explain fact was invalid", () => {
    renderTimeline(<ExplainAnalyzePanel events={[{ type: "explain_analyze", schema_version: 1 }]} />);
    expect(screen.getByText("Incomplete")).toBeTruthy();
    expect(screen.getByText(/delivery gap or unresolved stages/i)).toBeTruthy();
  });

  it("advances active stage bars between runtime events", () => {
    vi.useFakeTimers();
    try {
      renderTimeline(
        <ExplainAnalyzePanel
          events={[
            {
              ...identity,
              event_id: "turn:start",
              node_id: "turn",
              kind: "turn",
              label: "User turn",
              transition: "started",
              elapsed_ms: 0,
            },
            {
              ...identity,
              event_id: "attempt:start",
              node_id: "attempt",
              parent_node_id: "turn",
              kind: "provider_attempt",
              round_index: 0,
              attempt_index: 0,
              label: "Model request",
              transition: "started",
              elapsed_ms: 100,
            },
          ]}
        />,
      );

      const attempt = screen.getByRole("button", { name: /Model request/ });
      const bar = attempt;
      expect(bar).toBeTruthy();
      const initialWidth = Number.parseFloat(bar?.style.width ?? "");
      act(() => vi.advanceTimersByTime(1_000));
      const liveWidth = Number.parseFloat(bar?.style.width ?? "");

      expect(liveWidth).toBeGreaterThan(initialWidth);
      expect(attempt.getAttribute("aria-label")).toMatch(/elapsed so far/);
    } finally {
      vi.useRealTimers();
    }
  });

  it("freezes missing child timing when the turn terminates instead of inventing an end", () => {
    vi.useFakeTimers();
    try {
      const events = [
        { ...identity, event_id: "turn:start", node_id: "turn", kind: "turn", label: "User turn", transition: "started", elapsed_ms: 0 },
        { ...identity, event_id: "attempt:start", node_id: "attempt", parent_node_id: "turn", kind: "provider_attempt", round_index: 0, attempt_index: 0, label: "Model request", transition: "started", elapsed_ms: 100 },
      ];
      const view = renderTimeline(<ExplainAnalyzePanel events={events} />);
      act(() => vi.advanceTimersByTime(1_000));
      expect(screen.getByRole("button", { name: /Model request/ }).getAttribute("aria-label")).toContain("estimated elapsed so far");
      view.rerender(<ExplainAnalyzePanel events={[...events, {
        ...identity, event_id: "turn:finish", node_id: "turn", kind: "turn", label: "User turn", transition: "finished", elapsed_ms: 2_000, start_elapsed_ms: 0, duration_ms: 2_000, outcome: "completed",
      }]} />);
      const track = screen.getByRole("button", { name: /Model request/ });
      const frozenLabel = track.getAttribute("aria-label");
      expect(frozenLabel).toContain("Unknown");
      expect(frozenLabel).toContain("End not recorded");
      expect(frozenLabel).not.toContain("estimated");
      act(() => vi.advanceTimersByTime(60_000));
      expect(track.getAttribute("aria-label")).toBe(frozenLabel);
      expect(screen.getByText("Incomplete")).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });
});

function recordedStage(nodeId: string, label: string, start: number, end: number, extra = {}) {
  const base = { ...identity, node_id: nodeId, kind: "tool_call", label, ...extra };
  return [
    { ...base, event_id: `${nodeId}:start`, transition: "started", elapsed_ms: start },
    { ...base, event_id: `${nodeId}:finish`, transition: "finished", elapsed_ms: end,
      start_elapsed_ms: start, duration_ms: end - start, outcome: "completed" },
  ];
}

describe("Explain Analyze timeline inspection", () => {
  const facts = [
    ...recordedStage("turn", "User turn", 0, 2_000, { kind: "turn" }),
    ...recordedStage("cart", "Read cart", 500, 1_500, { parent_node_id: "turn" }),
    ...recordedStage("stock", "Read stock", 750, 1_250, { parent_node_id: "turn" }),
    ...recordedStage("answer", "Answer", 1_500, 2_000, {
      kind: "provider_attempt", parent_node_id: "turn", round_index: 0, attempt_index: 1,
      dependency_node_ids: ["cart", "stock"],
    }),
  ];

  it("preserves measured parallel intervals and selection while scrubbing and receiving facts", () => {
    const view = renderTimeline(<ExplainAnalyzePanel events={facts} />);
    const cart = screen.getByRole("button", { name: /Inspect Read cart/ });
    const stock = screen.getByRole("button", { name: /Inspect Read stock/ });
    expect(cart.style.left).toBe("25%");
    expect(cart.style.width).toBe("50%");
    expect(stock.style.left).toBe("37.5%");
    expect(stock.style.width).toBe("25%");
    fireEvent.click(screen.getByRole("button", { name: /Inspect Answer/ }));
    const details = screen.getByRole("complementary", { name: "Stage details: Answer" });
    expect(within(details).getByText(/Token usage not reported/)).toBeTruthy();
    fireEvent.click(within(details).getByRole("button", { name: "Read cart" }));
    expect(cart.getAttribute("aria-pressed")).toBe("true");
    fireEvent.change(screen.getByRole("slider"), { target: { value: "600" } });
    expect(stock.closest(".explain-analyze-lane")?.classList.contains("explain-analyze-lane-future")).toBe(true);
    expect(cart.style.width).toBe("50%");
    expect(screen.getByRole("complementary", { name: "Stage details: Read cart" })).toBeTruthy();
    view.rerender(<ExplainAnalyzePanel events={[...facts, ...recordedStage("saved", "Save", 2_000, 2_100)]} />);
    expect(screen.getByRole("button", { name: /Inspect Read cart/ }).getAttribute("aria-pressed")).toBe("true");
    expect((screen.getByRole("slider") as HTMLInputElement).value).toBe("600");
  });

  it("plays only on request, pauses and resets, and keeps separate clock-domain cursors", () => {
    vi.useFakeTimers();
    try {
      renderTimeline(<ExplainAnalyzePanel events={[...facts, ...recordedStage("child-turn", "Child turn", 0, 8_000, {
        kind: "turn", clock_domain_id: "z-child-clock", run_id: "child-run", turn_id: "child-turn",
      })]} />);
      const first = screen.getByRole("slider", { name: "Timeline 1 position" }) as HTMLInputElement;
      const second = screen.getByRole("slider", { name: "Timeline 2 position" }) as HTMLInputElement;
      act(() => vi.advanceTimersByTime(1_000));
      expect(first.value).toBe("2000");
      fireEvent.click(screen.getByRole("button", { name: "Play timeline 1" }));
      act(() => vi.advanceTimersByTime(500));
      expect(first.value).toBe("500");
      expect(second.value).toBe("8000");
      fireEvent.click(screen.getByRole("button", { name: "Pause timeline 1" }));
      act(() => vi.advanceTimersByTime(500));
      expect(first.value).toBe("500");
      fireEvent.click(screen.getByRole("button", { name: "Reset timeline 1" }));
      expect(first.value).toBe("0");
      fireEvent.click(screen.getByRole("button", { name: "Play timeline 1" }));
      act(() => vi.advanceTimersByTime(3_000));
      expect(first.value).toBe("2000");
      expect(screen.getByRole("button", { name: "Play timeline 1" })).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("Explain Analyze multi-domain lifecycle and windowing", () => {
  it("keeps a healthy child timeline live after the parent turn finishes", () => {
    renderTimeline(<ExplainAnalyzePanel events={[
      ...recordedStage("parent-turn", "Parent turn", 0, 2_000, { kind: "turn" }),
      {
        ...identity, event_id: "child:start", node_id: "child-turn", kind: "turn",
        label: "Child turn", transition: "started", elapsed_ms: 0,
        clock_domain_id: "z-child-clock", run_id: "child-run", turn_id: "child-turn",
      },
    ]} />);
    expect(screen.getByText("Live")).toBeTruthy();
    expect(screen.queryByText("Incomplete")).toBeNull();
    expect(screen.queryByText(/delivery gap or unresolved stages/)).toBeNull();
    expect(screen.getByRole("button", { name: /Inspect Child turn/ }).getAttribute("aria-label"))
      .toContain("estimated elapsed so far");
  });

  it("shares the initial 500-stage budget across timelines and preserves selection when paging", () => {
    const facts = Array.from({ length: 12 }, (_, domain) => {
      const clock = `clock-${String(domain).padStart(2, "0")}`;
      const turn = `turn-${domain}`;
      return [
        ...recordedStage(turn, `Turn ${domain}`, 0, 2_000, {
          kind: "turn", clock_domain_id: clock, turn_id: turn,
        }),
        ...Array.from({ length: 49 }, (_, call) => recordedStage(
          `${turn}/call-${call}`, `Call ${domain}/${call}`, call, call + 10,
          { parent_node_id: turn, clock_domain_id: clock, turn_id: turn },
        )).flat(),
      ];
    }).flat();
    const { container } = renderTimeline(<ExplainAnalyzePanel events={facts} />);
    expect(container.querySelectorAll(".explain-analyze-bar").length).toBe(500);
    expect(screen.queryByRole("slider", { name: "Timeline 11 position" })).toBeNull();
    const selected = screen.getByRole("button", { name: /Inspect Call 0\/0,/ });
    fireEvent.click(selected);
    fireEvent.click(screen.getByRole("button", { name: "Show more stages" }));
    expect(container.querySelectorAll(".explain-analyze-bar").length).toBe(600);
    expect(selected.getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("slider", { name: "Timeline 12 position" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Show more stages" })).toBeNull();
  });
});


describe("Explain Analyze tree view", () => {
  const facts = [
    ...recordedStage("tree-turn", "User turn", 0, 2_000, { kind: "turn" }),
    ...recordedStage("tree-call", "Read project configuration", 100, 800, { parent_node_id: "tree-turn" }),
  ];

  it("defaults to a readable execution tree with nearby measured details", () => {
    render(<ExplainAnalyzePanel events={facts} />);
    expect(screen.getByRole("button", { name: "Tree" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.queryByRole("slider")).toBeNull();
    expect(screen.queryByRole("button", { name: /Play timeline/ })).toBeNull();
    const stage = screen.getByRole("button", { name: /Inspect Read project configuration/ });
    expect(stage.classList.contains("explain-analyze-stage-title")).toBe(true);
    fireEvent.click(stage);
    const details = screen.getByRole("complementary", { name: "Stage details: Read project configuration" });
    expect(stage.closest(".explain-analyze-tree-node")?.contains(details)).toBe(true);
    expect(within(details).getByText("Measured duration")).toBeTruthy();
    expect(within(details).getByText("700 ms")).toBeTruthy();
  });

  it("preserves selection and collapsed branches when switching views", () => {
    render(<ExplainAnalyzePanel events={facts} />);
    fireEvent.click(screen.getByRole("button", { name: /Inspect Read project configuration/ }));
    fireEvent.click(screen.getByRole("button", { name: "Timeline" }));
    expect(screen.getByRole("slider")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Inspect Read project configuration/ }).getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "Collapse User turn" }));
    fireEvent.click(screen.getByRole("button", { name: "Tree" }));
    expect(screen.queryByRole("slider")).toBeNull();
    expect(screen.queryByRole("button", { name: /Inspect Read project configuration/ })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Expand User turn" }));
    expect(screen.getByRole("button", { name: /Inspect Read project configuration/ }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("complementary", { name: "Stage details: Read project configuration" })).toBeTruthy();
  });

  it("opens dependency details beyond the visible budget without rendering more stages", () => {
    const facts = [
      ...recordedStage("source", "Inspect result", 0, 2_000, {
        dependency_node_ids: ["hidden-dependency"],
      }),
      ...Array.from({ length: 500 }, (_, index) => recordedStage(
        `filler-${index}`, `Work ${index}`, index + 1, index + 2,
      )).flat(),
      ...recordedStage("hidden-dependency", "Dependency result", 1_000, 1_500),
    ];
    const { container, rerender } = render(<ExplainAnalyzePanel events={facts} />);
    expect(container.querySelectorAll(".explain-analyze-lane")).toHaveLength(500);
    fireEvent.click(screen.getByRole("button", { name: /Inspect Inspect result,/ }));
    fireEvent.click(within(screen.getByRole("complementary")).getByRole("button", { name: "Dependency result" }));
    const details = screen.getByRole("complementary", { name: "Stage details: Dependency result" });
    expect(within(details).getByText("500 ms")).toBeTruthy();
    expect(screen.getByText(/Selected stage is outside the visible rows/)).toBeTruthy();
    expect(container.querySelectorAll(".explain-analyze-lane")).toHaveLength(500);
    fireEvent.click(screen.getByRole("button", { name: "Timeline" }));
    rerender(<ExplainAnalyzePanel events={[...facts]} />);
    expect(screen.getByRole("complementary", { name: "Stage details: Dependency result" })).toBeTruthy();
    expect(container.querySelectorAll(".explain-analyze-lane")).toHaveLength(500);
    fireEvent.click(screen.getByRole("button", { name: "Show more stages" }));
    const selected = screen.getByRole("button", { name: /Inspect Dependency result,/ });
    expect(selected.getAttribute("aria-pressed")).toBe("true");
    expect(selected.closest(".explain-analyze-tree-node")?.contains(screen.getByRole("complementary"))).toBe(true);
    expect(screen.queryByText(/Selected stage is outside the visible rows/)).toBeNull();
  });
});

describe("Compact execution tree presentation", () => {
  it("shows one usage uncertainty note without four placeholder cards", () => {
    render(<ExplainAnalyzePanel events={recordedStage("turn", "User turn", 0, 2_000, { kind: "turn" })} />);
    expect(screen.getAllByText("Token usage not reported")).toHaveLength(1);
    expect(screen.queryByText("Not fully reported")).toBeNull();
    expect(screen.queryByText("Slowest model request")).toBeNull();
    expect(screen.queryByRole("slider")).toBeNull();
  });

  it("renders token and outcome columns from reported request facts", () => {
    render(<ExplainAnalyzePanel events={recordedStage("request", "Model request", 0, 1_000, {
      kind: "provider_attempt", round_index: 0, attempt_index: 0,
    }).map((fact) => fact.transition === "finished" ? { ...fact, usage: {
      basis: "provider_partial", fresh_input_tokens: 100, output_tokens: 12,
    } } : fact)} />);
    const node = screen.getByRole("button", { name: /Inspect Model request/ }).closest(".explain-analyze-tree-node")!;
    expect(node.querySelector(".explain-analyze-tree-usage")?.textContent).toContain("in 100");
    expect(node.querySelector(".explain-analyze-tree-status")?.textContent).toBe("Completed");
    fireEvent.click(within(node as HTMLElement).getByRole("button", { name: /Inspect Model request/ }));
    expect(screen.getByRole("complementary").textContent).toContain("Partial provider report");
  });
});

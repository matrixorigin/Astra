import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { vi } from "vitest";
import { ExplainAnalyzePanel } from "@/components/app/explain-analyze-panel";

function renderTimeline(element: Parameters<typeof render>[0]) {
  const view = render(element);
  const viewSwitch = screen.getByRole("group", { name: "Execution graph view" });
  fireEvent.click(within(viewSwitch).getByRole("button", { name: "Timeline" }));
  return view;
}

function selectView(name: "Tree" | "Timeline" | "Graph") {
  const viewSwitch = screen.getByRole("group", { name: "Execution graph view" });
  fireEvent.click(within(viewSwitch).getByRole("button", { name }));
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
  it.each([false, true])("shows unavailable auxiliary totals for conflicting captures (reverse=%s)", (reverse) => {
    const events = [100, 200].map((input, index) => ({
      ...identity, event_id: `turn-${index}:finish`, node_id: `turn-${index}`,
      kind: "turn" as const, label: "User turn", transition: "finished" as const,
      elapsed_ms: 10, start_elapsed_ms: 0, duration_ms: 10, outcome: "completed" as const,
      auxiliary_usage: {available: true, attempts: [{
        attempt_id: "same-attempt", provider: "typesafe", offering_id: "jev", model_name: "jev",
        purpose: "introspection", operation_id: "request_judgment", usage_status: "provider_exact" as const,
        usage: {basis: "provider_exact" as const, fresh_input_tokens: input},
      }]},
    }));
    render(<ExplainAnalyzePanel events={reverse ? events.reverse() : events} />);
    const usage = screen.getByLabelText("Auxiliary model token usage");
    expect(usage.textContent).toContain("capture unavailable");
    expect(usage.textContent).toContain("totals unavailable");
    expect(usage.textContent).not.toContain("requests reported");
    expect(usage.textContent).not.toContain("in 100");
  });

  it.each([false, true])("shows discarded incoming auxiliary conflict without any retained usage (reverse=%s)", (reverse) => {
    const base = {...identity, event_id: "same-event", node_id: "same-node", label: "Stage",
      transition: "finished" as const, elapsed_ms: 10, start_elapsed_ms: 0, duration_ms: 10,
      outcome: "completed" as const};
    const events = [
      {...base, kind: "preparation" as const},
      {...base, kind: "turn" as const, auxiliary_usage: {available: true, attempts: [{
        attempt_id: "same-attempt", provider: "typesafe", offering_id: "jev", model_name: "jev",
        purpose: "introspection", operation_id: "request_judgment", usage_status: "provider_exact" as const,
        usage: {basis: "provider_exact" as const, fresh_input_tokens: 100},
      }]}},
    ];
    render(<ExplainAnalyzePanel events={reverse ? events.reverse() : events} />);
    const usage = screen.getByLabelText("Auxiliary model token usage");
    expect(usage.textContent).toContain("capture unavailable");
    expect(usage.textContent).not.toContain("requests reported");
  });

  it("shows memory candidate decisions and distinguishes selection from injection", () => {
    const consoleError = vi.spyOn(console, "error");
    render(<ExplainAnalyzePanel events={[{
      ...identity, event_id: "context:finish", node_id: "context", kind: "context_assembly",
      label: "Assemble context sources", transition: "finished", elapsed_ms: 20,
      start_elapsed_ms: 0, duration_ms: 20, outcome: "completed",
      context: { assembly: { basis: "runtime_text_estimate", sources: [], edge_memory_selection: [{
        session_id: "s", turn: 1, operation: "relevance", method: "model", reason: "completed",
        model: "jev-test", elapsed_ms: 398, selection_order: [0], candidates: [
          { index: 0, selected: true, probability_bps: 9000 },
          { index: 1, selected: false, probability_bps: 1000 },
        ],
      }] } },
    }]} />);
    expect(screen.getAllByText(/2 candidates → 1 selected/).length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole("button", { name: /Inspect Assemble context sources/ }));
    expect(screen.getByText("Candidate 1")).toBeTruthy();
    expect(screen.getByText(/selected · model score 90.00%/)).toBeTruthy();
    expect(screen.getByText(/final prompt injection not measured/)).toBeTruthy();
    expect(consoleError).not.toHaveBeenCalled();
    consoleError.mockRestore();
  });

  it("distinguishes live approval and dispatch waits from running work", () => {
    renderTimeline(<ExplainAnalyzePanel live events={[
      { ...identity, event_id: "turn:start", node_id: "turn", kind: "turn", label: "User turn", transition: "started", elapsed_ms: 0 },
      { ...identity, event_id: "queue:start", node_id: "queue", parent_node_id: "turn", kind: "admission", label: "Waiting to dispatch bash", transition: "started", elapsed_ms: 1 },
      { ...identity, event_id: "approval:start", node_id: "approval", parent_node_id: "queue", kind: "wait", label: "Waiting for approval to run bash", transition: "started", elapsed_ms: 2 },
    ]} />);
    expect(screen.getByRole("button", { name: /Inspect Waiting to dispatch bash.*Awaiting dispatch/ }).className).toContain("bg-text-muted");
    expect(screen.getByRole("button", { name: /Inspect Waiting for approval to run bash.*Waiting$/ }).className).toContain("bg-warning");
    selectView("Tree");
    const waitingLane = screen.getByText("Waiting for approval to run bash").closest(".explain-analyze-lane");
    expect(waitingLane?.className).not.toContain("explain-analyze-tree-active");
    expect(waitingLane?.querySelector(".explain-analyze-tree-status")?.className).toContain("text-warning");
  });

  it("shows a plain-language overview, honest token lanes, and an expandable timeline", () => {
    renderTimeline(
      <ExplainAnalyzePanel live
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

    expect(screen.getByText("Explain Analyze · recorded timings and model usage")).toBeTruthy();
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
    renderTimeline(<ExplainAnalyzePanel live events={[]} degraded />);
    expect(screen.getByText(/delivery gap or unresolved stages/i)).toBeTruthy();
  });

  it("labels known instrumentation gaps separately from delivery failures", () => {
    render(
      <ExplainAnalyzePanel events={[{
        ...identity,
        event_id: "turn:finish",
        node_id: "turn",
        kind: "turn",
        label: "User turn",
        transition: "finished",
        elapsed_ms: 100,
        start_elapsed_ms: 0,
        duration_ms: 100,
        outcome: "completed",
        coverage_gaps: ["child_run_intervals", "tool_io_wait_intervals"],
      }]} />,
    );
    expect(screen.getByText("Complete")).toBeTruthy();
    expect(screen.getByLabelText("Explain Analyze coverage gaps").textContent).toContain(
      "child-run timing · tool I/O wait breakdown",
    );
    expect(screen.getByText("Observed overlap")).toBeTruthy();
  });

  it("shows a warning when every Explain fact was invalid", () => {
    renderTimeline(<ExplainAnalyzePanel live events={[{ type: "explain_analyze", schema_version: 1 }]} />);
    expect(screen.getByText("Incomplete")).toBeTruthy();
    expect(screen.getByText(/delivery gap or unresolved stages/i)).toBeTruthy();
  });

  it("advances active stage bars between runtime events", () => {
    vi.useFakeTimers();
    try {
      renderTimeline(
        <ExplainAnalyzePanel live
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
      const view = renderTimeline(<ExplainAnalyzePanel live events={events} />);
      act(() => vi.advanceTimersByTime(1_000));
      expect(screen.getByRole("button", { name: /Model request/ }).getAttribute("aria-label")).toContain("estimated elapsed so far");
      view.rerender(<ExplainAnalyzePanel live events={[...events, {
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
    const view = renderTimeline(<ExplainAnalyzePanel live events={facts} />);
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
    view.rerender(<ExplainAnalyzePanel live events={[...facts, ...recordedStage("saved", "Save", 2_000, 2_100)]} />);
    expect(screen.getByRole("button", { name: /Inspect Read cart/ }).getAttribute("aria-pressed")).toBe("true");
    expect((screen.getByRole("slider") as HTMLInputElement).value).toBe("600");
  });

  it("plays only on request, pauses and resets, and keeps separate clock-domain cursors", () => {
    vi.useFakeTimers();
    try {
      renderTimeline(<ExplainAnalyzePanel live events={[...facts, ...recordedStage("child-turn", "Child turn", 0, 8_000, {
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
    renderTimeline(<ExplainAnalyzePanel live events={[
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
    const { container } = renderTimeline(<ExplainAnalyzePanel live events={facts} />);
    expect(container.querySelectorAll(".explain-analyze-bar").length).toBe(500);
    expect(screen.queryByRole("slider", { name: "Timeline 11 position" })).toBeNull();
    // Query this worker's 50 stages rather than computing accessible names
    // for every button in all 12 worker timelines under JSDOM.
    const firstTimeline = container.querySelector<HTMLElement>(".explain-analyze-domain")!;
    const selected = within(firstTimeline).getByRole("button", { name: /Inspect Call 0\/0,/ });
    fireEvent.click(selected);
    const showMore = screen.getByText("Show more stages", { selector: "button" });
    fireEvent.click(showMore);
    expect(container.querySelectorAll(".explain-analyze-bar").length).toBe(600);
    expect(selected.getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("slider", { name: "Timeline 12 position" })).toBeTruthy();
    expect(screen.queryByText("Show more stages", { selector: "button" })).toBeNull();
  });
});


describe("Explain Analyze tree view", () => {
  const facts = [
    ...recordedStage("tree-turn", "User turn", 0, 2_000, { kind: "turn" }),
    ...recordedStage("tree-call", "Read project configuration", 100, 800, { parent_node_id: "tree-turn" }),
  ];

  it("defaults to a compact tree with inline measured details", () => {
    render(<ExplainAnalyzePanel live events={facts} />);
    expect(screen.getByRole("button", { name: "Tree" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.queryByRole("slider")).toBeNull();
    expect(screen.queryByRole("button", { name: /Play timeline/ })).toBeNull();
    const stage = screen.getByRole("button", { name: /Inspect Read project configuration/ });
    expect(stage.classList.contains("explain-analyze-stage-title")).toBe(true);
    expect(document.querySelector(".explain-analyze-mini-track")).toBeNull();
    fireEvent.click(stage);
    const details = screen.getByRole("complementary", { name: "Stage details: Read project configuration" });
    expect(stage.closest(".explain-analyze-tree-node")?.contains(details)).toBe(true);
    expect(within(details).getByText("Measured duration")).toBeTruthy();
    expect(within(details).getByText("700 ms")).toBeTruthy();
  });

  it("preserves selection and collapsed branches when switching views", () => {
    render(<ExplainAnalyzePanel live events={facts} />);
    selectView("Tree");
    fireEvent.click(screen.getByRole("button", { name: /Inspect Read project configuration/ }));
    selectView("Timeline");
    expect(screen.getByRole("slider")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Inspect Read project configuration/ }).getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "Collapse User turn" }));
    selectView("Tree");
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
    const { container, rerender } = render(<ExplainAnalyzePanel live events={facts} />);
    expect(container.querySelectorAll(".explain-analyze-lane")).toHaveLength(500);
    fireEvent.click(screen.getByText("Inspect result", { selector: "button" }));
    fireEvent.click(within(screen.getByRole("complementary")).getByRole("button", { name: "Dependency result" }));
    const details = screen.getByRole("complementary", { name: "Stage details: Dependency result" });
    expect(within(details).getByText("500 ms")).toBeTruthy();
    expect(screen.getByText(/Selected stage is outside the visible rows/)).toBeTruthy();
    expect(container.querySelectorAll(".explain-analyze-lane")).toHaveLength(500);
    selectView("Timeline");
    rerender(<ExplainAnalyzePanel live events={[...facts]} />);
    expect(screen.getByRole("complementary", { name: "Stage details: Dependency result" })).toBeTruthy();
    expect(container.querySelectorAll(".explain-analyze-lane")).toHaveLength(500);
    fireEvent.click(screen.getByText("Show more stages", { selector: "button" }));
    const dependencyRow = container.querySelector<HTMLElement>('[data-tree-node-id="hidden-dependency"]')!;
    const selected = within(dependencyRow).getByRole("button", { name: /Inspect Dependency result,/ });
    expect(selected.getAttribute("aria-pressed")).toBe("true");
    expect(selected.closest(".explain-analyze-tree-node")?.contains(screen.getByRole("complementary"))).toBe(true);
    expect(screen.queryByText(/Selected stage is outside the visible rows/)).toBeNull();
  });
});

describe("Explain Analyze graph view", () => {
  it("renders explicit containment and dependency edges with forked cards", () => {
    const facts = [
      ...recordedStage("turn", "User turn", 0, 2_000, { kind: "turn" }),
      ...recordedStage("first", "Read project configuration", 100, 700, { parent_node_id: "turn" }),
      ...recordedStage("second", "Read source files", 150, 800, { parent_node_id: "turn" }),
      ...recordedStage("merge", "Prepare answer", 900, 1_400, {
        parent_node_id: "turn", dependency_node_ids: ["first", "second"],
      }),
    ];
    const { container } = render(<ExplainAnalyzePanel events={facts} />);
    selectView("Graph");
    expect(screen.getByRole("button", { name: "Graph" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByLabelText("Graph legend")).toBeTruthy();
    const edges = [...container.querySelectorAll(".explain-analyze-graph-edge")];
    expect(edges).toHaveLength(5);
    expect(container.querySelectorAll(".explain-analyze-graph-edge-dependency")).toHaveLength(2);
    expect(edges.some((edge) => edge.getAttribute("d")?.includes("first") ?? false)).toBe(false);

    const turn = container.querySelector('[data-node-id="turn"]') as HTMLElement;
    const first = container.querySelector('[data-node-id="first"]') as HTMLElement;
    const second = container.querySelector('[data-node-id="second"]') as HTMLElement;
    expect(Number.parseFloat(turn.style.left)).toBeLessThan(Number.parseFloat(first.style.left));
    expect(Number.parseFloat(first.style.top)).toBeLessThan(Number.parseFloat(second.style.top));
    expect(screen.getByRole("button", { name: /Inspect Read project configuration/ })).toBe(first);

    expect(screen.getByRole("button", { name: "Zoom out graph" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "Zoom out graph" }));
    expect(screen.getByText("90%")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Fit graph" }));
    expect(screen.getByText("78%")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /Inspect Prepare answer/ }));
    expect(screen.getByRole("complementary", { name: "Stage details: Prepare answer" })).toBeTruthy();
  });

  it("keeps wait styling and worker clock domains separate", () => {
    const wait = recordedStage("wait", "Waiting for approval", 100, 400, { kind: "wait" });
    const child = recordedStage("child", "Child worker", 0, 500, {
      kind: "turn", clock_domain_id: "child-clock", run_id: "child-run", turn_id: "child-turn",
    });
    const { container } = render(<ExplainAnalyzePanel live events={[...wait, ...child]} />);
    selectView("Graph");
    expect(container.querySelector('[data-node-id="wait"]')?.className).toContain("graph-card-wait");
    expect(container.querySelectorAll(".explain-analyze-graph-domain")).toHaveLength(2);
    expect(screen.getByText("Measured wait time")).toBeTruthy();
    expect(screen.getByTitle("300 ms")).toBeTruthy();
  });
});

describe("Compact execution tree presentation", () => {
  it("shows one usage uncertainty note without four placeholder cards", () => {
    render(<ExplainAnalyzePanel live events={recordedStage("turn", "User turn", 0, 2_000, { kind: "turn" })} />);
    expect(screen.getAllByText("Token usage not reported")).toHaveLength(1);
    expect(screen.queryByText("Not fully reported")).toBeNull();
    expect(screen.queryByText("Slowest model request")).toBeNull();
    expect(screen.queryByRole("slider")).toBeNull();
  });

  it("renders token and outcome columns from reported request facts", () => {
    render(<ExplainAnalyzePanel live events={recordedStage("request", "Model request", 0, 1_000, {
      kind: "provider_attempt", round_index: 0, attempt_index: 0,
    }).map((fact) => fact.transition === "finished" ? { ...fact, usage: {
      basis: "provider_partial", fresh_input_tokens: 100, output_tokens: 12,
    } } : fact)} />);
    selectView("Tree");
    const node = screen.getByRole("button", { name: /Inspect Model request/ }).closest(".explain-analyze-tree-node")!;
    expect(node.querySelector(".explain-analyze-tree-usage")?.textContent).toContain("in 100");
    expect(node.querySelector(".explain-analyze-tree-status")?.textContent).toBe("Completed");
    fireEvent.click(within(node as HTMLElement).getByRole("button", { name: /Inspect Model request/ }));
    expect(screen.getByRole("complementary").textContent).toContain("Partial provider report");
  });
});

it("shows request budget and assembly source estimates without claiming provider usage", () => {
  const terminal = { ...identity, transition: "finished", start_elapsed_ms: 0,
    elapsed_ms: 100, duration_ms: 100, outcome: "completed" };
  render(<ExplainAnalyzePanel live events={[
    { ...terminal, event_id: "t", node_id: "turn", kind: "turn", label: "Answer" },
    { ...terminal, event_id: "p", node_id: "prep", parent_node_id: "turn", kind: "preparation", label: "Prepare request",
      context: { budget: { basis: "pre_provider_estimate", estimated_input_tokens: 4200,
        estimated_system_tokens: 1400, tool_schema_tokens: 900, requested_output_tokens: 2000,
        reserved_protocol_tokens: 300, effective_input_limit_tokens: 12000,
        model_context_limit_tokens: 16000, visible_tool_count: 8 } } },
    { ...terminal, event_id: "c", node_id: "context", parent_node_id: "turn", kind: "context_assembly", label: "Prepare context",
      context: { assembly: { basis: "runtime_text_estimate", sources: [
        { kind: "memory", section_count: 2, estimated_tokens: 210 },
        { kind: "project_context", section_count: 1, estimated_tokens: 340 },
      ] } } },
  ]}/>);
  expect(screen.getByText("Token usage not reported")).toBeTruthy();
  expect(screen.getByText("Input ≈4,200 / 12,000")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: /Inspect Prepare request,/ }));
  const budget = screen.getByRole("region", { name: "Request budget" });
  expect(within(budget).getByText("4,200 tokens")).toBeTruthy();
  expect(within(budget).getByText("Output allowance")).toBeTruthy();
  expect(within(budget).getByText(/Not billed usage/)).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: /Inspect Prepare context,/ }));
  const sources = screen.getByRole("region", { name: "Context sources" });
  expect(within(sources).getByText("Retrieved memory")).toBeTruthy();
  expect(within(sources).getByText("210 tokens · 2 sections")).toBeTruthy();
  expect(within(sources).getByText(/Later request preparation may change/)).toBeTruthy();
  expect(screen.getByText("Token usage not reported")).toBeTruthy();
});

it("keeps a historical started-only node static until live observation is explicit", () => {
  const facts = [{
    type: "explain_analyze", schema_version: 1, event_id: "open-start",
    run_id: "run", turn_id: "turn", node_id: "open", producer_id: "worker",
    clock_domain_id: "clock", kind: "turn", label: "Unfinished turn",
    transition: "started", elapsed_ms: 0,
  }];
  const { container, rerender } = render(<ExplainAnalyzePanel events={facts} />);
  expect(screen.getByText("Snapshot")).toBeInTheDocument();
  expect(screen.queryByText("Active stages")).toBeNull();
  expect(screen.getByText("End not recorded")).toBeInTheDocument();
  selectView("Tree");
  expect(container.querySelector(".explain-analyze-tree-active")).toBeNull();
  rerender(<ExplainAnalyzePanel events={facts} live />);
  expect(screen.getByText("Live")).toBeInTheDocument();
  expect(container.querySelector(".explain-analyze-tree-active")).not.toBeNull();
  rerender(<ExplainAnalyzePanel events={facts} />);
  expect(container.querySelector(".explain-analyze-tree-active")).toBeNull();
});

it("keeps another turn live on the same clock after the first turn ends", () => {
  const base = { type: "explain_analyze", schema_version: 1, run_id: "run",
    producer_id: "worker", clock_domain_id: "shared", kind: "turn" };
  const { container } = render(<ExplainAnalyzePanel live events={[
    { ...base, event_id: "a-end", node_id: "a", turn_id: "a", label: "Closed turn",
      transition: "finished", elapsed_ms: 100, start_elapsed_ms: 0, duration_ms: 100, outcome: "completed" },
    { ...base, event_id: "b-start", node_id: "b", turn_id: "b", label: "Active turn",
      transition: "started", elapsed_ms: 110 },
  ]} />);
  expect(screen.getByText("Live")).toBeInTheDocument();
  expect(screen.queryByText("End not recorded")).toBeNull();
  selectView("Tree");
  expect(container.querySelectorAll(".explain-analyze-tree-active")).toHaveLength(1);
});

it("shows coverage for input/output-only reports even when another request omits usage", () => {
  render(<ExplainAnalyzePanel events={[
    ...recordedStage("with-usage", "First request", 0, 100, {
      kind: "provider_attempt", round_index: 0, attempt_index: 0,
      usage: { basis: "provider_partial", fresh_input_tokens: 40, output_tokens: 2 },
    }),
    ...recordedStage("without-usage", "Second request", 100, 200, {
      kind: "provider_attempt", round_index: 0, attempt_index: 1,
    }),
  ]} />);
  expect(screen.getByText("Reported subtotal · 1/2 requests · partial or estimated")).toBeInTheDocument();
  expect(screen.getByText("40")).toBeInTheDocument();
  expect(screen.getByText("Token usage partly reported")).toBeInTheDocument();
});


describe("Text tree navigation and sharing", () => {
  const facts = [
    ...recordedStage("turn", "Review project", 0, 2_000, { kind: "turn" }),
    ...recordedStage("batch", "Verify changes", 100, 1_800, { kind: "tool_batch", parent_node_id: "turn" }),
    ...recordedStage("test", "Run unit tests", 200, 800, { parent_node_id: "batch" }),
    ...recordedStage("lint", "Check formatting", 900, 1_600, { parent_node_id: "batch" }),
  ];

  it("preserves collapsed branches across search and incoming facts", () => {
    const { rerender } = render(<ExplainAnalyzePanel events={facts} />);
    fireEvent.click(screen.getByRole("button", { name: "Collapse Verify changes" }));
    expect(screen.queryByRole("button", { name: /Inspect Run unit tests/ })).toBeNull();
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "unit tests" } });
    expect(screen.getByRole("button", { name: /Inspect Run unit tests/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Inspect Check formatting/ })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Clear search" }));
    expect(screen.queryByRole("button", { name: /Inspect Run unit tests/ })).toBeNull();
    rerender(<ExplainAnalyzePanel events={[...facts, ...recordedStage("later", "Prepare final answer", 1800, 1900, { parent_node_id: "turn" })]} />);
    expect(screen.queryByRole("button", { name: /Inspect Run unit tests/ })).toBeNull();
    expect(screen.getByRole("button", { name: /Inspect Prepare final answer/ })).toBeTruthy();
  });

  it("navigates visible rows and expands a branch without moving focus", () => {
    render(<ExplainAnalyzePanel events={facts} />);
    const root = screen.getByRole("button", { name: /Inspect Review project/ });
    root.focus();
    fireEvent.keyDown(root, { key: "ArrowDown" });
    const batch = screen.getByRole("button", { name: /Inspect Verify changes/ });
    expect(document.activeElement).toBe(batch);
    fireEvent.keyDown(batch, { key: "ArrowLeft" });
    expect(screen.queryByRole("button", { name: /Inspect Run unit tests/ })).toBeNull();
    expect(document.activeElement).toBe(batch);
    fireEvent.keyDown(batch, { key: "ArrowRight" });
    expect(screen.getByRole("button", { name: /Inspect Run unit tests/ })).toBeTruthy();
    expect(document.activeElement).toBe(batch);
    fireEvent.keyDown(batch, { key: "End" });
    expect(document.activeElement).toBe(screen.getByRole("button", { name: /Inspect Check formatting/ }));
  });

  it("copies the full recorded hierarchy even while the display is collapsed", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
    render(<ExplainAnalyzePanel events={facts} />);
    fireEvent.click(screen.getByRole("button", { name: "Collapse all" }));
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Copy tree" })); });
    expect(writeText).toHaveBeenCalledOnce();
    expect(writeText.mock.calls[0][0]).toContain("Run unit tests");
    expect(writeText.mock.calls[0][0]).toMatch(/[├└]─/);
    expect(screen.getByText("Tree copied")).toBeTruthy();
  });
});

it("retains visible descendants, focus and inline selection when live siblings cross the old expansion threshold", () => {
  const facts = [
    ...recordedStage("root", "Large run", 0, 2_000, { kind: "turn" }),
    ...recordedStage("batch", "Check source", 0, 800, { kind: "tool_batch", parent_node_id: "root" }),
    ...recordedStage("child", "Focused test", 100, 500, { parent_node_id: "batch" }),
    ...Array.from({ length: 248 }, (_, i) => recordedStage(`sibling-${i}`, `Sibling ${i}`, 900, 1000, { parent_node_id: "root" })).flat(),
  ];
  const { rerender } = render(<ExplainAnalyzePanel events={facts} />);
  const child = screen.getByText("Focused test", { selector: "button" });
  child.focus();
  fireEvent.click(child);
  rerender(<ExplainAnalyzePanel events={[...facts, ...recordedStage("new", "New sibling", 1100, 1200, { parent_node_id: "root" })]} />);
  expect(screen.getByText("Focused test", { selector: "button" })).toBe(child);
  expect(document.activeElement).toBe(child);
  expect(child.getAttribute("aria-pressed")).toBe("true");
  expect(screen.getByRole("complementary", { name: "Stage details: Focused test" })).toBeTruthy();
});

it("retains known token subtotals when another request omits the lane", () => {
  const attempts = [
    ...recordedStage("a", "First request", 0, 100, { kind: "provider_attempt", round_index: 0, attempt_index: 0 }).map((fact) => fact.transition === "finished" ? { ...fact, usage: { basis: "provider_partial", fresh_input_tokens: 40, output_tokens: 2 } } : fact),
    ...recordedStage("b", "Second request", 110, 200, { kind: "provider_attempt", round_index: 0, attempt_index: 1 }).map((fact) => fact.transition === "finished" ? { ...fact, usage: { basis: "provider_partial", output_tokens: 3 } } : fact),
  ];
  render(<ExplainAnalyzePanel events={attempts} />);
  const summary = screen.getByLabelText("Model token usage");
  expect(summary.textContent).toContain("Fresh input 40(1/2 requests)");
  expect(summary.textContent).toContain("Output 5");
  expect(summary.textContent).not.toContain("Cache read 0");
});

it("labels the maximum of independent turns without claiming a run wall time", () => {
  render(<ExplainAnalyzePanel events={[
    ...recordedStage("first", "First turn", 0, 2_000, { kind: "turn" }),
    ...recordedStage("second", "Second turn", 0, 3_000, { kind: "turn", clock_domain_id: "clock-2", turn_id: "turn-2" }),
  ]} />);
  expect(screen.getByText("Longest turn")).toBeTruthy();
  expect(screen.queryByText("Turn time")).toBeNull();
  expect(screen.getByTitle("3.0 s")).toBeTruthy();
});

it("keeps the external replay gap warning in copied text even when surviving facts are consistent", async () => {
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
  render(<ExplainAnalyzePanel degraded events={recordedStage("turn", "Completed root", 0, 100, { kind: "turn" })} />);
  expect(screen.getByText("Incomplete")).toBeTruthy();
  await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Copy tree" })); });
  expect(writeText).toHaveBeenCalledOnce();
  expect(writeText.mock.calls[0][0]).toContain("Incomplete observation: delivery gap");
  expect(writeText.mock.calls[0][0]).toContain("Structural integrity: consistent");
});

import type { WorkTaskGraphItemV2, WorkTaskGraphPageV2 } from "@astra/sdk";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";

const refresh = vi.fn();
const loadPage = vi.fn();
const refreshHead = vi.fn();

vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh }),
}));

vi.mock("@/app/(workspace)/works/[workId]/actions", () => ({
  loadWorkTaskGraphPageAction: (...args: unknown[]) => loadPage(...args),
  refreshWorkTaskGraphAction: (...args: unknown[]) => refreshHead(...args),
}));

import { WorkTaskGraph } from "@/components/app/work-task-graph";

function item(
  itemId: string,
  objective: string,
  execution: WorkTaskGraphItemV2["execution"] = {
    status: "not_started",
    terminal: false,
    run: null,
  },
  verification: WorkTaskGraphItemV2["verification"] = {
    status: "unknown",
    latest_check: null,
  },
  delivery: WorkTaskGraphItemV2["delivery"] = {
    status: "unreported",
    summary: null,
    blocker_kind: null,
    unavailable_capabilities: [],
  },
): WorkTaskGraphItemV2 {
  return {
    item_id: itemId,
    revision: 1,
    kind: itemId === "root" ? "milestone" : "task",
    objective,
    expected_result: `${objective} is reviewable`,
    declaration_state: "active",
    execution,
    delivery,
    verification,
  };
}

function page(overrides: Partial<WorkTaskGraphPageV2> = {}): WorkTaskGraphPageV2 {
  return {
    schema_version: 2,
    scope: "declared_work",
    basis: {
      work_id: "work-1",
      work_revision: 2,
      goal_revision: 1,
      goal: "Ship the change",
      criteria_set_revision: 1,
      criteria_member_count: 1,
      criteria_manifest_hash: `sha256:${"a".repeat(64)}`,
      branch_id: "branch-1",
      branch_revision: 2,
      branch_goal_revision: 1,
      branch_criteria_set_revision: 1,
      branch_basis_graph_revision: 1,
      graph_revision: 2,
      graph_item_count: 2,
      graph_edge_count: 1,
      graph_manifest_hash: `sha256:${"b".repeat(64)}`,
    },
    cursor: { graph_revision: 2, item_offset: 0, dependency_offset: 0 },
    next_cursor: { graph_revision: 2, item_offset: 1, dependency_offset: 1 },
    items: {
      offset: 0,
      limit: 1,
      total: 2,
      entries: [item("root", "Prepare implementation")],
    },
    dependencies: {
      offset: 0,
      limit: 1,
      total: 1,
      entries: [
        {
          predecessor_item_id: "root",
          successor_item_id: "verify",
          kind: "dependency",
        },
      ],
    },
    ...overrides,
  };
}

beforeEach(() => {
  refresh.mockReset();
  loadPage.mockReset();
  refreshHead.mockReset();
});

test("keeps execution completion distinct from current verification evidence", () => {
  const completed = item("root", "Implement change", {
    status: "completed",
    terminal: true,
    run: {
      run_id: "run-1",
      attempt_id: "run-1",
      graph_revision: 2,
      run_generation: 3,
      last_event_idx: 9,
      updated_at: "2026-08-03T00:00:00Z",
    },
  }, undefined, {
    status: "delivered",
    summary: "Implementation delivered.",
    blocker_kind: null,
    unavailable_capabilities: [],
  });
  const initial = page({
    next_cursor: null,
    items: { offset: 0, limit: 8, total: 1, entries: [completed] },
    dependencies: { offset: 0, limit: 128, total: 0, entries: [] },
  });

  render(<WorkTaskGraph initial={initial} />);

  expect(screen.getByText("Needs verification")).toBeInTheDocument();
  expect(screen.queryByText("Verified")).not.toBeInTheDocument();
  expect(
    screen.getByRole("button", { name: "Needs attention 1" }),
  ).toBeInTheDocument();
  expect(refreshHead).not.toHaveBeenCalled();
});

test("does not let a terminal run without typed delivery disappear from Current", () => {
  const terminal = item("root", "Implement change", {
    status: "completed",
    terminal: true,
    run: {
      run_id: "run-1",
      attempt_id: "run-1",
      graph_revision: 2,
      run_generation: 1,
      last_event_idx: 4,
      updated_at: "2026-08-03T00:00:04Z",
    },
  });
  render(
    <WorkTaskGraph
      initial={page({
        next_cursor: null,
        items: { offset: 0, limit: 8, total: 1, entries: [terminal] },
        dependencies: { offset: 0, limit: 128, total: 0, entries: [] },
      })}
    />,
  );

  expect(screen.getByText("Result not reported")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Current 1" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Needs attention 1" })).toBeInTheDocument();
});

test("renders a typed blocked delivery and its actionable capability fact", () => {
  const blocked = item(
    "root",
    "Fetch release metadata",
    {
      status: "completed",
      terminal: true,
      run: {
        run_id: "run-1",
        attempt_id: "run-1",
        graph_revision: 2,
        run_generation: 1,
        last_event_idx: 4,
        updated_at: "2026-08-03T00:00:04Z",
      },
    },
    undefined,
    {
      status: "blocked",
      summary: "The remote fetch capability is unavailable.",
      blocker_kind: "capability_unavailable",
      unavailable_capabilities: ["web_fetch"],
    },
  );
  render(
    <WorkTaskGraph
      initial={page({
        next_cursor: null,
        items: { offset: 0, limit: 8, total: 1, entries: [blocked] },
        dependencies: { offset: 0, limit: 128, total: 0, entries: [] },
      })}
    />,
  );

  expect(screen.getByText("Blocked")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Details" }));
  expect(screen.getByText(/remote fetch capability is unavailable/i)).toBeInTheDocument();
  expect(screen.getByText(/unavailable: web_fetch/i)).toBeInTheDocument();
});

test("updates execution state while the Work turn is still live", async () => {
  const running = item("root", "Prepare implementation", {
    status: "running",
    terminal: false,
    run: {
      run_id: "run-1",
      attempt_id: "run-1",
      graph_revision: 2,
      run_generation: 1,
      last_event_idx: 3,
      updated_at: "2026-08-03T00:00:03Z",
    },
  });
  refreshHead.mockResolvedValue({
    ok: true,
    page: page({
      items: { offset: 0, limit: 1, total: 2, entries: [running] },
    }),
  });

  render(<WorkTaskGraph initial={page()} live />);

  expect(screen.getByText("Live")).toBeInTheDocument();
  expect(await screen.findByText("Working")).toBeInTheDocument();
  expect(screen.getByText("Updated")).toBeInTheDocument();
  expect(refreshHead).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
  });
});

test("observes typed delivery settlement without requiring a graph revision change", async () => {
  const basis = { ...page().basis, graph_item_count: 1, graph_edge_count: 0 };
  const terminal = item("root", "Fetch release metadata", {
    status: "completed",
    terminal: true,
    run: {
      run_id: "run-1",
      attempt_id: "run-1",
      graph_revision: 2,
      run_generation: 1,
      last_event_idx: 4,
      updated_at: "2026-08-03T00:00:04Z",
    },
  });
  const blocked = {
    ...terminal,
    delivery: {
      status: "blocked" as const,
      summary: "The remote capability is unavailable.",
      blocker_kind: "capability_unavailable" as const,
      unavailable_capabilities: ["web_fetch"],
    },
  };
  refreshHead.mockResolvedValue({
    ok: true,
    page: page({
      basis,
      next_cursor: null,
      items: { offset: 0, limit: 8, total: 1, entries: [blocked] },
      dependencies: { offset: 0, limit: 128, total: 0, entries: [] },
    }),
  });

  render(
    <WorkTaskGraph
      initial={page({
        basis,
        next_cursor: null,
        items: { offset: 0, limit: 8, total: 1, entries: [terminal] },
        dependencies: { offset: 0, limit: 128, total: 0, entries: [] },
      })}
      live
    />,
  );

  expect(await screen.findByText("Blocked")).toBeInTheDocument();
  expect(screen.getByText("Updated")).toBeInTheDocument();
});

test("observes a read-only Work quietly without duplicating the initial read", async () => {
  vi.useFakeTimers();
  try {
    refreshHead.mockResolvedValue({ ok: true, page: page() });
    render(<WorkTaskGraph initial={page()} />);

    expect(refreshHead).not.toHaveBeenCalled();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(29_999);
    });
    expect(refreshHead).not.toHaveBeenCalled();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(refreshHead).toHaveBeenCalledTimes(1);
  } finally {
    vi.useRealTimers();
  }
});

test("loads the exact pinned continuation and exposes dependency identities", async () => {
  loadPage.mockResolvedValue({
    ok: true,
    page: page({
      cursor: { graph_revision: 2, item_offset: 1, dependency_offset: 1 },
      next_cursor: null,
      items: {
        offset: 1,
        limit: 1,
        total: 2,
        entries: [item("verify", "Verify behavior")],
      },
      dependencies: { offset: 1, limit: 1, total: 1, entries: [] },
    }),
  });
  render(<WorkTaskGraph initial={page()} />);

  fireEvent.click(screen.getByRole("button", { name: "Show more plan items" }));

  expect(await screen.findByText("Verify behavior")).toBeInTheDocument();
  expect(loadPage).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    cursor: { graph_revision: 2, item_offset: 1, dependency_offset: 1 },
  });
  fireEvent.click(screen.getAllByRole("button", { name: "Details" })[1]!);
  expect(screen.getAllByText("Prepare implementation")).toHaveLength(2);
  expect(screen.getByText("Blocked by")).toBeInTheDocument();
});

test("rejects a continuation from another graph instead of splicing revisions", async () => {
  const drifted = page({
    basis: {
      ...page().basis,
      graph_revision: 3,
      graph_manifest_hash: `sha256:${"c".repeat(64)}`,
    },
    cursor: { graph_revision: 2, item_offset: 1, dependency_offset: 1 },
  });
  loadPage.mockResolvedValue({ ok: true, page: drifted });
  render(<WorkTaskGraph initial={page()} />);

  fireEvent.click(screen.getByRole("button", { name: "Show more plan items" }));

  expect(
    await screen.findByText(/plan changed while this page was open/i),
  ).toBeInTheDocument();
  expect(screen.queryByText("Verify behavior")).not.toBeInTheDocument();
});

test("surfaces an incomplete terminal pagination response as inconsistent", async () => {
  loadPage.mockResolvedValue({
    ok: true,
    page: page({
      basis: { ...page().basis, graph_item_count: 3 },
      cursor: { graph_revision: 2, item_offset: 1, dependency_offset: 1 },
      next_cursor: null,
      items: { offset: 1, limit: 1, total: 3, entries: [item("verify", "Verify")] },
      dependencies: { offset: 1, limit: 1, total: 1, entries: [] },
    }),
  });
  render(
    <WorkTaskGraph
      initial={page({
        basis: { ...page().basis, graph_item_count: 3 },
        items: {
          offset: 0,
          limit: 1,
          total: 3,
          entries: [item("root", "Prepare implementation")],
        },
      })}
    />,
  );

  fireEvent.click(screen.getByRole("button", { name: "Show more plan items" }));

  await waitFor(() =>
    expect(screen.getByRole("alert")).toHaveTextContent(/inconsistent/i),
  );
});

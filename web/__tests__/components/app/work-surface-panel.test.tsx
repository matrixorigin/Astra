import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { WorkSurfacePanel } from "@/components/app/work-surface-panel";
import { createEmptyWorkSurface } from "@/lib/work-surface";

vi.mock("lucide-react", () => {
  const Icon = () => null;
  return {
    __esModule: true,
    Activity: Icon,
    AlertTriangle: Icon,
    Bot: Icon,
    ChevronRight: Icon,
    CheckCircle2: Icon,
    Circle: Icon,
    ClipboardList: Icon,
    Loader2: Icon,
    Pause: Icon,
    RotateCw: Icon,
    Terminal: Icon,
    Wrench: Icon,
    X: Icon,
  };
});

describe("WorkSurfacePanel", () => {
  it("renders concise run status text in the panel header", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          runStatus: "running",
        }}
        activeRun={{ runId: "run-1", status: "running" }}
        tab="tasks"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    expect(screen.getByText("Thinking")).toBeInTheDocument();
    expect(screen.queryByText("Run running")).not.toBeInTheDocument();
  });

  it("orders same-status tasks by most recent update first", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          tasks: [
            {
              id: "task-a",
              title: "Older pending task",
              status: "pending",
              created_at: "2026-06-10T00:00:00.000Z",
              updated_at: "2026-06-10T00:00:00.000Z",
            },
            {
              id: "task-b",
              title: "Newer pending task",
              status: "pending",
              created_at: "2026-06-10T00:00:00.000Z",
              updated_at: "2026-06-10T00:01:00.000Z",
            },
          ],
        }}
        activeRun={{ runId: "run-1", status: "running" }}
        tab="tasks"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    const newer = screen.getByText("Newer pending task");
    const older = screen.getByText("Older pending task");

    expect(
      newer.compareDocumentPosition(older) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  it("orders tool cards by most recent activity first", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          tools: [
            {
              callId: "call-long",
              tool: "long_running_bash",
              arguments: '{"cmd":"npm test"}',
              result: "done",
              status: "done",
              startedAt: 1_000,
              finishedAt: 9_000,
            },
            {
              callId: "call-short",
              tool: "recently_started_grep",
              arguments: '{"pattern":"TODO"}',
              status: "running",
              startedAt: 5_000,
            },
          ],
        }}
        activeRun={{ runId: "run-1", status: "running" }}
        tab="tools"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    const completedLater = screen.getByText("long_running_bash");
    const startedEarlier = screen.getByText("recently_started_grep");

    expect(
      completedLater.compareDocumentPosition(startedEarlier) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  it("focuses tool failures when an activity link opens the tools tab", async () => {
    const user = userEvent.setup();
    const state = {
      ...createEmptyWorkSurface("session-1", "run-1"),
      hydrated: true,
      tools: [
        {
          callId: "call-ok",
          tool: "read_file",
          result: "ok",
          status: "done" as const,
          startedAt: 1_000,
          finishedAt: 2_000,
        },
        {
          callId: "call-failed",
          tool: "bash",
          result: "Edge transport disconnected",
          status: "error" as const,
          errorKind: "transport_disconnected",
          blocked: true,
          startedAt: 3_000,
          finishedAt: 4_000,
        },
      ],
    };

    const { rerender } = render(
      <WorkSurfacePanel
        state={state}
        activeRun={{ runId: "run-1", status: "running" }}
        tab="tasks"
        openSignal={0}
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    rerender(
      <WorkSurfacePanel
        state={state}
        activeRun={{ runId: "run-1", status: "running" }}
        tab="tools"
        openSignal={1}
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    expect(await screen.findAllByText("Needs attention")).not.toHaveLength(0);
    await waitFor(() => {
      expect(screen.getAllByText("bash").length).toBeGreaterThan(0);
      expect(screen.queryByText("read_file")).not.toBeInTheDocument();
    });

    await user.click(
      screen.getAllByRole("button", { name: /All\s*tools\s*2/ })[0],
    );
    expect(screen.getAllByText("read_file").length).toBeGreaterThan(0);
  });

  it("orders subagent cards by most recent update and opens the latest details", async () => {
    const loadAgentRun = vi.fn().mockResolvedValue({
      runId: "run-new",
      sessionId: "session-1",
      status: "running",
      workspace: {
        kind: "edge_workspace",
        display_name: "MacBook Pro",
        cwd: "/Users/xupeng/github/astra",
        authority: "read_write",
        fallback_policy: "disabled",
      },
      executor: {
        kind: "edge_agent",
        executor_id: "edge-macbook-1",
        display_name: "MacBook Pro",
        transport: "edge_ws",
        status: "online",
      },
      transport: "edge_ws",
      fallbackPolicy: "disabled",
      events: [
        {
          type: "agent_live_event",
          event_kind: "output_delta",
          content: "new child live output",
        },
      ],
      generatedAt: "2026-06-11T00:00:00.000Z",
    });

    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          agents: [
            {
              agentId: "agent-old",
              runId: "run-old",
              agentType: "explore",
              description: "Older investigation",
              status: "completed",
              updatedAt: 1_000,
            },
            {
              agentId: "agent-new",
              runId: "run-new",
              agentType: "code-review",
              description: "Latest review",
              status: "running",
              events: [
                {
                  id: "event-output",
                  type: "agent_live_event:output_delta",
                  label: "Output",
                  detail: "latest card live output",
                  tone: "running",
                  timestamp: 2_000,
                },
              ],
              updatedAt: 2_000,
            },
          ],
        }}
        activeRun={{ runId: "run-1", status: "running" }}
        tab="agents"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={loadAgentRun}
      />,
    );

    const latest = await screen.findByText("Latest review");
    const older = screen.getByText("Older investigation");
    expect(
      latest.compareDocumentPosition(older) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(screen.getByText("Live output")).toBeInTheDocument();
    expect(screen.getAllByText("latest card live output").length).toBeGreaterThan(0);
    expect(screen.getByText("Runtime")).toBeInTheDocument();
    expect(screen.getAllByText("MacBook Pro").length).toBeGreaterThan(0);
    expect(screen.getByText("Files")).toBeInTheDocument();
    expect(
      screen.getAllByText("/Users/xupeng/github/astra").length,
    ).toBeGreaterThan(0);
    expect(screen.getByText("Connection")).toBeInTheDocument();
    expect(screen.getByText("edge ws")).toBeInTheDocument();
    expect(screen.getByText("Policy")).toBeInTheDocument();
    expect(screen.getByText("disabled")).toBeInTheDocument();
    await waitFor(() => {
      expect(loadAgentRun).toHaveBeenCalledWith("run-new");
    });
  });

  it("renders cancelled tool cards without a failure notice", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          tools: [
            {
              callId: "call-cancelled",
              tool: "bash",
              result: "Tool 'bash' cancelled before completion",
              status: "cancelled",
              errorKind: "cancelled",
              startedAt: 1_000,
              finishedAt: 2_000,
            },
          ],
        }}
        activeRun={{ runId: "run-1", status: "cancelled" }}
        tab="tools"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    expect(screen.getAllByText("cancelled").length).toBeGreaterThan(0);
    expect(
      screen.getByText("Tool 'bash' cancelled before completion"),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Tool blocked|Tool timed out/)).toBeNull();
  });

  it("renders unavailable workspace executor tool failures with actionable copy", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          tools: [
            {
              callId: "call-cloud",
              tool: "bash",
              result:
                "Error: workspace 'Cloud checkout' is not routed to an available executor transport.",
              status: "error",
              errorKind: "workspace_executor_unavailable",
              blocked: true,
              workspace: {
                kind: "git_checkout",
                display_name: "Cloud checkout",
                cwd: "/checkout/repo",
                authority: "read_only",
                fallback_policy: "disabled",
              },
              executor: {
                kind: "orchestrator_managed",
                executor_id: "orchestrator-managed",
                display_name: "Orchestrator-managed executor",
                transport: "sandbox_resident_agent",
                status: "degraded",
              },
              transport: "sandbox_resident_agent",
              fallbackPolicy: "disabled",
              startedAt: 1_000,
              finishedAt: 2_000,
            },
          ],
        }}
        activeRun={{ runId: "run-1", status: "blocked" }}
        tab="tools"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    expect(screen.getByText("Needs file environment")).toBeInTheDocument();
    expect(
      screen.getByText(
        "This request needs a file or command environment. Connect one or choose a sandbox, then retry.",
      ),
    ).toBeInTheDocument();
    expect(screen.getByText("Orchestrator-managed executor")).toBeInTheDocument();
    expect(screen.getByText("/checkout/repo")).toBeInTheDocument();
  });

  it("shows selected subagent live output as a readable transcript", async () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          agents: [
            {
              agentId: "agent-live",
              agentType: "code-review",
              description: "Review current branch",
              status: "running",
              events: [
                {
                  id: "event-spawned",
                  type: "agent_spawned",
                  label: "Spawned",
                  tone: "running",
                  timestamp: 1_000,
                },
                {
                  id: "event-output",
                  type: "agent_live_event:output_delta",
                  label: "Output",
                  detail: "child review result: no critical issues",
                  tone: "running",
                  timestamp: 2_000,
                },
              ],
              updatedAt: 2_000,
            },
          ],
        }}
        activeRun={{ runId: "run-1", status: "running" }}
        tab="agents"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    expect(await screen.findByText("Live output")).toBeInTheDocument();
    expect(
      screen.getAllByText("child review result: no critical issues").length,
    ).toBeGreaterThan(0);
  });
});

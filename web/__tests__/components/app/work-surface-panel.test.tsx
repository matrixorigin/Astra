import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { WorkSurfacePanel } from "@/components/app/work-surface-panel";
import { WORKSPACE_EXECUTION_BLOCKED_MESSAGE } from "@/lib/run-status-messages";
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
    Sparkles: Icon,
    Terminal: Icon,
    Wrench: Icon,
    X: Icon,
  };
});

describe("WorkSurfacePanel", () => {
  it("loads durable reflection and decision evidence on demand", async () => {
    const user = userEvent.setup();
    const onLoadInsights = vi.fn().mockResolvedValue({
      sessionId: "session-1",
      audit: null,
      reflection: {
        session_id: "session-1",
        focus: "auto",
        overview: {},
        diagnoses: [],
        insights: [],
        recommendations: ["Verify the failing integration path next."],
      },
      decisionTrace: {
        session_id: "session-1",
        focus: "tool_surface",
        overview: { selected_route: "edge workspace" },
        diagnoses: [],
        insights: [],
        recommendations: [],
      },
      warnings: ["audit: temporarily unavailable"],
      generatedAt: "2026-07-16T10:00:00.000Z",
    });

    function Surface() {
      const [tab, setTab] = useState<"tasks" | "agents" | "tools" | "insights">(
        "tasks",
      );
      return (
        <WorkSurfacePanel
          state={{
            ...createEmptyWorkSurface("session-1", "run-1"),
            hydrated: true,
          }}
          tab={tab}
          onTabChange={setTab}
          onRefresh={vi.fn()}
          onLoadAgentRun={vi.fn()}
          onLoadInsights={onLoadInsights}
        />
      );
    }

    render(<Surface />);
    await user.click(screen.getByRole("button", { name: /Insights/i }));

    expect(onLoadInsights).toHaveBeenCalledTimes(1);
    expect(
      await screen.findByText("Verify the failing integration path next."),
    ).toBeInTheDocument();
    expect(screen.getByText("edge workspace")).toBeInTheDocument();
    expect(screen.getByText("Partial evidence")).toBeInTheDocument();
  });

  it("renders incomplete insight evidence without crashing", async () => {
    const user = userEvent.setup();
    const onLoadInsights = vi.fn().mockResolvedValue({
      sessionId: "session-1",
      audit: null,
      reflection: {
        session_id: "session-1",
        focus: "auto",
        overview: {},
      },
      decisionTrace: null,
      warnings: [],
      generatedAt: "2026-07-16T10:00:00.000Z",
    });

    function Surface() {
      const [tab, setTab] = useState<"tasks" | "agents" | "tools" | "insights">(
        "tasks",
      );
      return (
        <WorkSurfacePanel
          state={{
            ...createEmptyWorkSurface("session-1", "run-1"),
            hydrated: true,
          }}
          tab={tab}
          onTabChange={setTab}
          onRefresh={vi.fn()}
          onLoadAgentRun={vi.fn()}
          onLoadInsights={onLoadInsights}
        />
      );
    }

    render(<Surface />);
    await user.click(screen.getByRole("button", { name: /Insights/i }));

    expect(await screen.findByText("Reflect")).toBeInTheDocument();
    expect(
      screen.getByText(
        "No recommendations were produced for the current evidence.",
      ),
    ).toBeInTheDocument();
  });

  it("opens a child run as a canonical transcript workspace", async () => {
    const user = userEvent.setup();
    const onLoadAgentRun = vi.fn().mockResolvedValue({
      runId: "child-run-1",
      sessionId: "session-1",
      status: "completed",
      events: [],
      transcript: [
        {
          session_id: "session-1",
          item_seq: 1,
          run_id: "child-run-1",
          role: "user",
          content: "Review the concurrency boundary.",
          created_at: "2026-07-16T10:00:00.000Z",
        },
        {
          session_id: "session-1",
          item_seq: 2,
          run_id: "child-run-1",
          role: "assistant",
          content: "The boundary is race-safe.",
          reasoning: "Checked the durable CAS path.",
          created_at: "2026-07-16T10:00:02.000Z",
        },
      ],
      transcriptComplete: true,
      transcriptWarning: null,
      generatedAt: "2026-07-16T10:00:03.000Z",
    });

    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "root-run"),
          hydrated: true,
          agents: [
            {
              agentId: "agent-1",
              runId: "child-run-1",
              parentRunId: "root-run",
              agentType: "code-review",
              description: "Concurrency review",
              status: "completed",
              updatedAt: Date.parse("2026-07-16T10:00:03.000Z"),
            },
          ],
        }}
        tab="agents"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={onLoadAgentRun}
      />,
    );

    await waitFor(() =>
      expect(onLoadAgentRun).toHaveBeenCalledWith("child-run-1"),
    );
    await user.click(
      await screen.findByRole("button", { name: /Open transcript/i }),
    );

    expect(
      screen.getByRole("dialog", { name: "Concurrency review transcript" }),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Review the concurrency boundary."),
    ).toBeInTheDocument();
    expect(screen.getByText("The boundary is race-safe.")).toBeInTheDocument();
    expect(
      screen.getByText("Checked the durable CAS path."),
    ).toBeInTheDocument();
  });

  it("switches between durable parent and child agent transcripts without closing the workspace", async () => {
    const user = userEvent.setup();
    const onLoadAgentRun = vi.fn(async (runId: string) => ({
      runId,
      sessionId: "session-1",
      status: "completed",
      events: [],
      transcript: [
        {
          session_id: "session-1",
          item_seq: 1,
          run_id: runId,
          role: "assistant",
          content:
            runId === "run-review"
              ? "Review coordinator result"
              : "Nested security finding",
          created_at: "2026-07-16T10:00:02.000Z",
        },
      ],
      transcriptComplete: true,
      transcriptWarning: null,
      generatedAt: "2026-07-16T10:00:03.000Z",
    }));

    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "root-run"),
          hydrated: true,
          agents: [
            {
              agentId: "agent-review",
              runId: "run-review",
              parentRunId: "root-run",
              description: "Review coordinator",
              status: "completed",
              updatedAt: 2_000,
            },
            {
              agentId: "agent-security",
              runId: "run-security",
              parentRunId: "run-review",
              description: "Security review",
              status: "completed",
              updatedAt: 3_000,
            },
          ],
        }}
        tab="agents"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={onLoadAgentRun}
      />,
    );

    expect(screen.getByText("Child of Main conversation")).toBeInTheDocument();
    expect(screen.getByText("Child of Review coordinator")).toBeInTheDocument();
    await user.click(
      await screen.findByRole("button", { name: /Open transcript/i }),
    );
    expect(
      await screen.findByText("Review coordinator result"),
    ).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: /Security review, Completed/i }),
    );
    expect(
      await screen.findByText("Nested security finding"),
    ).toBeInTheDocument();
    expect(onLoadAgentRun).toHaveBeenCalledWith("run-security");

    await user.click(screen.getByRole("button", { name: "Main conversation" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

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

  it("never fabricates task completion when no measured progress exists", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          tasks: [
            {
              id: "task-unmeasured",
              title: "Investigate the production failure",
              status: "in_progress",
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

    expect(
      screen.getByText("Investigate the production failure"),
    ).toBeInTheDocument();
    expect(screen.queryByText("45%")).not.toBeInTheDocument();
    expect(screen.getByText(/Updated/)).toBeInTheDocument();
  });

  it("names the actual tasks that block downstream work", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          tasks: [
            {
              id: "schema",
              title: "Apply schema migration",
              status: "in_progress",
              blocks: ["api"],
              created_at: "2026-06-10T00:00:00.000Z",
              updated_at: "2026-06-10T00:01:00.000Z",
            },
            {
              id: "api",
              title: "Start API rollout",
              status: "pending",
              blocked_by: ["schema"],
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

    expect(screen.getByText("Blocked by")).toBeInTheDocument();
    expect(screen.getAllByText("Apply schema migration")).toHaveLength(2);
    expect(screen.getByText("Unblocks 1")).toBeInTheDocument();
  });

  it("labels measured subtask completion as evidence-backed progress", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          tasks: [
            {
              id: "task-measured",
              title: "Ship the runtime repair",
              status: "in_progress",
              created_at: "2026-06-10T00:00:00.000Z",
              updated_at: "2026-06-10T00:01:00.000Z",
              subtasks: [
                { id: "sub-1", title: "Fix", status: "completed" },
                { id: "sub-2", title: "Verify", status: "pending" },
              ],
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

    expect(screen.getByText("Subtask completion")).toBeInTheDocument();
    expect(screen.getByText("50%")).toBeInTheDocument();
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

  it("keeps successful tool evidence compact until the user expands it", async () => {
    const user = userEvent.setup();
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          tools: [
            {
              callId: "call-read",
              tool: "read_file",
              arguments: '{"path":"src/main.rs"}',
              result: "fn main() {}",
              status: "done",
              startedAt: 1_000,
              finishedAt: 2_000,
              durationMs: 1_000,
            },
          ],
        }}
        tab="tools"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    expect(screen.queryByText("Args")).not.toBeInTheDocument();
    expect(screen.queryByText("Result")).not.toBeInTheDocument();
    await user.click(
      screen.getByRole("button", { name: "Expand read_file details" }),
    );
    expect(screen.getByText("Args")).toBeInTheDocument();
    expect(screen.getByText("Result")).toBeInTheDocument();
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

  it("keeps subagent card order stable while opening active details", async () => {
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

    const older = screen.getByText("Older investigation");
    const latest = await screen.findByText("Latest review");
    expect(
      older.compareDocumentPosition(latest) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(screen.getAllByText("Output").length).toBeGreaterThan(0);
    expect(
      screen.getAllByText("latest card live output").length,
    ).toBeGreaterThan(0);
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

  it("presents agent turns as budget usage rather than completion progress", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          agents: [
            {
              agentId: "agent-budget",
              agentType: "research",
              description: "Investigate the incident",
              status: "running",
              turn: 2,
              maxTurns: 8,
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

    expect(screen.getByText("Turn budget")).toBeInTheDocument();
    expect(screen.getByText("2/8")).toBeInTheDocument();
    expect(screen.queryByText("25%")).not.toBeInTheDocument();
  });

  it("opens the first active subagent instead of chasing latest timestamps", async () => {
    const loadAgentRun = vi.fn().mockResolvedValue({
      runId: "run-first",
      sessionId: "session-1",
      status: "running",
      events: [
        {
          type: "agent_live_event",
          event_kind: "output_delta",
          content: "first active details",
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
              agentId: "agent-first",
              runId: "run-first",
              agentType: "research",
              description: "First active agent",
              status: "running",
              updatedAt: 1_000,
            },
            {
              agentId: "agent-latest",
              runId: "run-latest",
              agentType: "research",
              description: "Latest active agent",
              status: "running",
              updatedAt: 9_000,
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

    expect(await screen.findByText("First active agent")).toBeInTheDocument();
    expect(screen.getByText("Latest active agent")).toBeInTheDocument();
    await waitFor(() => {
      expect(loadAgentRun).toHaveBeenCalledWith("run-first");
    });
    expect(loadAgentRun).not.toHaveBeenCalledWith("run-latest");
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
      screen.getByText(WORKSPACE_EXECUTION_BLOCKED_MESSAGE),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Orchestrator-managed executor"),
    ).toBeInTheDocument();
    expect(screen.getByText("/checkout/repo")).toBeInTheDocument();
  });

  it("renders parent-paused subagents as waiting instead of interrupted", () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          agents: [
            {
              agentId: "agent-paused-child",
              agentType: "code-review",
              description: "Review current branch",
              status: "waiting",
              reason: "parent_run_paused",
              resultSummary:
                "Parent run paused before a terminal subagent event was observed.",
              updatedAt: 1_801_000_000_000,
            },
          ],
        }}
        activeRun={{ runId: "run-1", status: "paused" }}
        tab="agents"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    expect(screen.getAllByText("Waiting").length).toBeGreaterThan(0);
    expect(
      screen.getAllByText(
        "Parent run paused before a terminal subagent event was observed.",
      ).length,
    ).toBeGreaterThan(0);
    expect(screen.queryByText("Needs attention")).toBeNull();
    expect(screen.queryByText("Interrupted")).toBeNull();
  });

  it("renders empty-completion subagents as needing a final answer instead of failed", async () => {
    render(
      <WorkSurfacePanel
        state={{
          ...createEmptyWorkSurface("session-1", "run-1"),
          hydrated: true,
          agents: [
            {
              agentId: "agent-weather",
              agentType: "research",
              description: "Fetch Shanghai weather",
              status: "interrupted",
              reason: "empty_completion",
              resultSummary: "上海今日小雨，33°C / 25°C。",
              updatedAt: 1_801_000_000_000,
              events: [
                {
                  id: "event-weather",
                  type: "agent_interrupted",
                  label: "Needs final answer",
                  detail: "上海今日小雨，33°C / 25°C。",
                  tone: "warning",
                  timestamp: 1_801_000_000_000,
                },
              ],
            },
          ],
        }}
        activeRun={{ runId: "run-1", status: "paused" }}
        tab="agents"
        onTabChange={vi.fn()}
        onRefresh={vi.fn()}
        onLoadAgentRun={vi.fn()}
      />,
    );

    expect(screen.getAllByText("Needs final answer").length).toBeGreaterThan(0);
    expect(screen.queryByText("Interrupted")).toBeNull();
    expect(screen.getByText("Needs attention")).toBeInTheDocument();
    expect(await screen.findByText("No final answer")).toBeInTheDocument();
    expect(
      screen.getAllByText("上海今日小雨，33°C / 25°C。").length,
    ).toBeGreaterThan(0);
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

    await screen.findAllByText("Output");
    expect(
      screen.getAllByText("child review result: no critical issues").length,
    ).toBeGreaterThan(0);
  });
});

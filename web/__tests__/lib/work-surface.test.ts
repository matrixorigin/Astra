import {
  applyWorkSurfaceEvent,
  createEmptyWorkSurface,
  hydrateWorkSurface,
} from "@/lib/work-surface";

const task = {
  id: "task-1",
  title: "Implement panel",
  status: "in_progress",
  created_at: "2026-06-10T00:00:00.000Z",
  updated_at: "2026-06-10T00:00:00.000Z",
};

describe("work surface reducer", () => {
  it("applies task board snapshots as the authoritative task state", () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface(), {
      type: "task_board_snapshot",
      session_id: "session-1",
      reason: "task_update",
      tasks: [task],
    });

    expect(state.sessionId).toBe("session-1");
    expect(state.hydrated).toBe(true);
    expect(state.loading).toBe(false);
    expect(state.tasks).toEqual([task]);
  });

  it("tracks current-protocol tool calls through completion", () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface("session-1"), {
      type: "tool_call",
      tool_call: {
        id: "call-1",
        function: {
          name: "bash",
          arguments: { command: "echo hi" },
        },
      },
    });
    state = applyWorkSurfaceEvent(state, {
      type: "tool_call_end",
      call_id: "call-1",
      result: "Error: denied",
      success: false,
    });

    expect(state.tools).toHaveLength(1);
    expect(state.tools[0]).toMatchObject({
      callId: "call-1",
      tool: "bash",
      arguments: "{\"command\":\"echo hi\"}",
      result: "Error: denied",
      status: "error",
    });
  });

  it("hydrates from tasks and current run events", () => {
    const state = hydrateWorkSurface(createEmptyWorkSurface(), {
      sessionId: "session-1",
      tasks: [task],
      events: [
        {
          type: "agent_spawned",
          agent_id: "agent-1",
          run_id: "run-2",
          parent_run_id: "run-1",
          agent_type: "reviewer",
          description: "Audit changes",
        },
        {
          type: "agent_completed",
          agent_id: "agent-1",
          result_summary: "No blockers",
          total_tool_calls: 3,
          duration_ms: 42,
        },
      ],
      generatedAt: "2026-06-10T00:00:00.000Z",
    });

    expect(state.tasks).toEqual([task]);
    expect(state.agents).toHaveLength(1);
    expect(state.agents[0]).toMatchObject({
      agentId: "agent-1",
      agentType: "reviewer",
      description: "Audit changes",
      resultSummary: "No blockers",
      status: "completed",
      totalToolCalls: 3,
      durationMs: 42,
    });
    expect(state.agents[0].events?.map((event) => event.label)).toEqual([
      "Spawned",
      "Completed",
    ]);
  });

  it("updates subagents from live current-protocol progress events", () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface(), {
      type: "agent_delegated",
      agent_id: "agent-2",
      task: "Explore task API",
    });
    state = applyWorkSurfaceEvent(state, {
      type: "agent_progress",
      agent_id: "agent-2",
      status: "tool_executing",
      tool_name: "grep",
      turn: 2,
      max_turns: 5,
      total_tool_calls: 4,
      total_tokens: { prompt: 100, completion: 20 },
    });

    expect(state.agents[0]).toMatchObject({
      agentId: "agent-2",
      description: "Explore task API",
      status: "tool_executing",
      toolName: "grep",
      turn: 2,
      maxTurns: 5,
      totalToolCalls: 4,
      totalPromptTokens: 100,
      totalCompletionTokens: 20,
    });
    expect(state.agents[0].events?.at(-1)).toMatchObject({
      label: "Running grep",
      detail: "turn 2/5, 4 tools, 120 tokens",
      tone: "running",
    });
  });
});

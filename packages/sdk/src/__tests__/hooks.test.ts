/**
 * @vitest-environment jsdom
 */
import { renderHook, act } from "@testing-library/react";
import { useAstraChat, useAstraRun } from "../hooks";
import { AstraClient } from "../client";
import type { SSEClient } from "../sse-client";
import type { StreamEvent, ConnectionState } from "../types";

// ─── Mock AstraClient ──────────────────────────────────────────────

function createMockClient() {
  const client = new AstraClient({ baseUrl: "http://localhost" });

  // Mock streamChat to call onEvent callbacks synchronously
  const streamChatMock = vi.fn();
  client.streamChat = streamChatMock;

  // Mock run polling
  const getRunStatusMock = vi.fn();
  const getRunEventsMock = vi.fn();
  client.getRunStatus = getRunStatusMock;
  client.getRunEvents = getRunEventsMock;

  return { client, streamChatMock, getRunStatusMock, getRunEventsMock };
}

/**
 * Helper: set up streamChat mock to fire a sequence of events.
 * Returns a fake SSEClient with a close() spy.
 */
function mockStreamEvents(streamChatMock: ReturnType<typeof vi.fn>, events: StreamEvent[]) {
  const closeSpy = vi.fn();

  streamChatMock.mockImplementation(
    (
      _params: unknown,
      opts: {
        onEvent: (e: StreamEvent) => void;
        onStateChange?: (s: ConnectionState) => void;
      },
    ) => {
      // Fire state changes and events synchronously
      opts.onStateChange?.("connecting");
      opts.onStateChange?.("connected");
      for (const event of events) {
        opts.onEvent(event);
      }
      opts.onStateChange?.("disconnected");
      return { close: closeSpy } as unknown as SSEClient;
    },
  );

  return closeSpy;
}

// ─── useAstraChat ──────────────────────────────────────────────────

describe("useAstraChat", () => {
  test("initial state is idle with empty arrays", () => {
    const { client } = createMockClient();
    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    expect(result.current.sessionId).toBeNull();
    expect(result.current.runId).toBeNull();
    expect(result.current.messages).toEqual([]);
    expect(result.current.toolCalls).toEqual([]);
    expect(result.current.isStreaming).toBe(false);
    expect(result.current.error).toBeNull();
    expect(result.current.plan).toBeNull();
    expect(result.current.runStatus).toBeNull();
    expect(result.current.waitingFor).toBeNull();
    expect(result.current.workspace).toBeUndefined();
    expect(result.current.executor).toBeUndefined();
    expect(result.current.transport).toBeNull();
    expect(result.current.fallbackPolicy).toBeNull();
    expect(result.current.connectionState).toBe("idle");
    expect(result.current.agentEvents).toEqual([]);
    expect(result.current.tasks).toEqual([]);
    expect(result.current.agents).toEqual([]);
  });

  test("sendMessage adds user + assistant placeholder", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, []);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hello");
    });

    // Should have user message + empty assistant placeholder
    expect(result.current.messages).toHaveLength(2);
    expect(result.current.messages[0].role).toBe("user");
    expect(result.current.messages[0].content).toBe("Hello");
    expect(result.current.messages[1].role).toBe("assistant");
    expect(result.current.messages[1].streaming).toBe(true);
  });

  test("sendMessage passes the complete integration boundary to streamChat", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, []);

    const skillSearch = {
      dynamicSurface: true,
      minCatalogSize: 8,
      surfaceCap: 14,
    };
    const { result } = renderHook(() =>
      useAstraChat({
        client,
        model: "test-model",
        allowSkills: ["s1"],
        allowTools: ["bash"],
        skillSearch,
        agentBinding: {
          id: "binding-1",
          capabilityServerRefs: { mcp: "tools", skills: "skills" },
        },
        runtimeProfile: "agent_binding_registry",
        executionBudget: { initialTurns: 3, hardTurnLimit: 12 },
        capabilities: ["multi_agent", "reflect"],
        explain: true,
        context: { source: "embedded-web-agent" },
        workspaceBinding: {
          kind: "server_sandbox",
          authority: "read_write",
        },
        executorBinding: {
          kind: "server_local",
          status: "online",
        },
      }),
    );

    act(() => {
      result.current.sendMessage("Hello");
    });

    expect(streamChatMock).toHaveBeenCalledTimes(1);
    const req = streamChatMock.mock.calls[0][0] as {
      allowSkills?: string[];
      allowTools?: string[];
      skillSearch?: typeof skillSearch;
      selectedModel?: { model: string };
      agentBinding?: {
        id: string;
        capabilityServerRefs: { mcp: string; skills: string };
      };
      runtimeProfile?: string;
      executionBudget?: { initialTurns?: number; hardTurnLimit?: number };
      capabilities?: string[];
      explain?: boolean;
      context?: Record<string, unknown>;
      workspaceBinding?: { kind: string };
      executorBinding?: { kind: string };
    };
    expect(req.selectedModel).toEqual({ model: "test-model" });
    expect(req.allowSkills).toEqual(["s1"]);
    expect(req.allowTools).toEqual(["bash"]);
    expect(req.skillSearch).toEqual(skillSearch);
    expect(req.agentBinding).toEqual({
      id: "binding-1",
      capabilityServerRefs: { mcp: "tools", skills: "skills" },
    });
    expect(req.runtimeProfile).toBe("agent_binding_registry");
    expect(req.executionBudget).toEqual({
      initialTurns: 3,
      hardTurnLimit: 12,
    });
    expect(req.capabilities).toEqual(["multi_agent", "reflect"]);
    expect(req.explain).toBe(true);
    expect(req.context).toEqual({ source: "embedded-web-agent" });
    expect(req.workspaceBinding).toMatchObject({ kind: "server_sandbox" });
    expect(req.executorBinding).toMatchObject({ kind: "server_local" });
  });

  test("processes session_info event", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "session_info",
        session_id: "sess-1",
        run_id: "run-1",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    expect(result.current.sessionId).toBe("sess-1");
    expect(result.current.runId).toBe("run-1");
  });

  test("projects task board and agent lifecycle for embedded workspaces", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "task_board_snapshot",
        session_id: "s1",
        tasks: [
          {
            id: "task-1",
            title: "Review implementation",
            status: "in_progress",
            created_at: "2026-07-16T00:00:00.000Z",
            updated_at: "2026-07-16T00:00:00.000Z",
          },
        ],
      } as StreamEvent,
      {
        type: "agent_spawned",
        agent_id: "agent-1",
        run_id: "run-child",
        parent_run_id: "run-parent",
        agent_type: "code-review",
        description: "Review correctness",
        timestamp: 100,
      } as StreamEvent,
      {
        type: "agent_completed",
        agent_id: "agent-1",
        status: "completed",
        result_summary: "No critical findings",
        total_tool_calls: 4,
        timestamp: 200,
      } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client, model: "test-model" }),
    );
    act(() => {
      result.current.sendMessage("Review");
    });

    expect(result.current.tasks).toEqual([
      expect.objectContaining({
        id: "task-1",
        status: "in_progress",
      }),
    ]);
    expect(result.current.agents).toEqual([
      expect.objectContaining({
        agentId: "agent-1",
        runId: "run-child",
        parentRunId: "run-parent",
        status: "completed",
        resultSummary: "No critical findings",
        totalToolCalls: 4,
        updatedAt: 200,
      }),
    ]);
    expect(result.current.agentEvents).toHaveLength(2);
  });

  test("processes text_delta events to build assistant content", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: "text_delta", content: "Hello " } as StreamEvent,
      { type: "text_delta", content: "World" } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    const assistantMsg = result.current.messages.find(
      (m) => m.role === "assistant",
    );
    expect(assistantMsg?.content).toBe("Hello World");
  });

  test("ignores late events from superseded streams", () => {
    const { client, streamChatMock } = createMockClient();
    const closeSpy = vi.fn();
    const callbacks: Array<(event: StreamEvent) => void> = [];
    streamChatMock.mockImplementation(
      (
        _params: unknown,
        opts: {
          onEvent: (event: StreamEvent) => void;
        },
      ) => {
        callbacks.push(opts.onEvent);
        return { close: closeSpy } as unknown as SSEClient;
      },
    );

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("first");
      result.current.sendMessage("second");
    });
    expect(callbacks).toHaveLength(2);

    act(() => {
      callbacks[0]({ type: "text_delta", content: "STALE" } as StreamEvent);
      callbacks[1]({ type: "text_delta", content: "fresh" } as StreamEvent);
    });

    const assistantMessages = result.current.messages.filter(
      (message) => message.role === "assistant",
    );
    expect(assistantMessages).toHaveLength(2);
    expect(assistantMessages[0].content).toBe("");
    expect(assistantMessages[1].content).toBe("fresh");
  });

  /** Aligns with real-world-scenarios / streamChat ordering (session → text → usage → complete). */
  test("processes session_info → text_deltas → usage → turn_complete", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: "session_info", session_id: "s-w", run_id: "r-w" } as StreamEvent,
      { type: "text_delta", content: "Ok" } as StreamEvent,
      {
        type: "usage",
        prompt_tokens: 1,
        completion_tokens: 2,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
      } as StreamEvent,
      { type: "turn_complete" } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Go");
    });

    expect(result.current.sessionId).toBe("s-w");
    expect(result.current.runId).toBe("r-w");
    const assistant = result.current.messages.find(
      (m) => m.role === "assistant",
    );
    expect(assistant?.content).toBe("Ok");
    expect(assistant?.streaming).toBe(false);
    expect(result.current.usage.totalTokens).toBe(3);
  });

  test("processes tool_call_start and tool_call_end", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "tool_call_start",
        call_id: "tc-1",
        tool: "bash",
        arguments: '{"command":"ls"}',
      } as StreamEvent,
      {
        type: "tool_call_end",
        call_id: "tc-1",
        result: "file1\nfile2",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("list files");
    });

    expect(result.current.toolCalls).toHaveLength(1);
    expect(result.current.toolCalls[0].tool).toBe("bash");
    expect(result.current.toolCalls[0].status).toBe("done");
    expect(result.current.toolCalls[0].result).toBe("file1\nfile2");
  });

  test("preserves skipped tool_call_end status", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "tool_call_start",
        call_id: "tc-skip",
        tool: "read_file",
        arguments: '{"path":"README.md"}',
      } as StreamEvent,
      {
        type: "tool_call_end",
        call_id: "tc-skip",
        status: "skipped",
        skipped: true,
        success: true,
        result: "Duplicate call skipped.",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("read twice");
    });

    expect(result.current.toolCalls).toHaveLength(1);
    expect(result.current.toolCalls[0].status).toBe("skipped");
    expect(result.current.toolCalls[0].result).toBe("Duplicate call skipped.");
  });

  test("projects raw tool_call and transport lifecycle into one bound tool card", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "tool_call",
        tool_call: {
          id: "tc-edge-1",
          function: {
            name: "edge_shell",
            arguments: '{"command":"pwd"}',
          },
        },
        workspace: {
          kind: "edge_workspace",
          display_name: "MacBook Pro",
          cwd: "/Users/test/project",
          authority: "read_write",
          fallback_policy: "disabled",
        },
        executor: {
          kind: "edge_agent",
          executor_id: "edge-1",
          display_name: "MacBook Pro",
          transport: "edge_ws",
          status: "online",
        },
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
      {
        type: "tool_routing_decision",
        call_id: "tc-edge-1",
        tool: "edge_shell",
        route: "edge_workspace",
        transport: "edge_ws",
      } as StreamEvent,
      {
        type: "tool_transport_completed",
        call_id: "tc-edge-1",
        tool: "edge_shell",
        result: { stdout: "/Users/test/project" },
        success: true,
        duration_ms: 12,
        transport: "edge_ledger",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("pwd");
    });

    expect(result.current.toolCalls).toHaveLength(1);
    expect(result.current.toolCalls[0]).toMatchObject({
      callId: "tc-edge-1",
      tool: "edge_shell",
      arguments: '{"command":"pwd"}',
      result: '{"stdout":"/Users/test/project"}',
      status: "done",
      workspace: {
        kind: "edge_workspace",
        cwd: "/Users/test/project",
      },
      executor: {
        kind: "edge_agent",
        executor_id: "edge-1",
      },
      transport: "edge_ledger",
      fallbackPolicy: "disabled",
      route: "edge_workspace",
      durationMs: 12,
    });
  });

  test("tracks run execution boundary and execution-blocked waiting state", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "run_started",
        run_id: "run-edge-1",
        session_id: "session-edge-1",
        workspace: {
          kind: "edge_workspace",
          display_name: "MacBook Pro",
          cwd: "/Users/test/project",
          authority: "read_write",
          fallback_policy: "disabled",
        },
        executor: {
          kind: "edge_agent",
          executor_id: "edge-1",
          display_name: "MacBook Pro",
          transport: "edge_ws",
          status: "online",
        },
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
      {
        type: "run_waiting",
        run_id: "run-edge-1",
        reason: "waiting: executor_offline",
        workspace: {
          kind: "edge_workspace",
          cwd: "/Users/test/project",
        },
        executor: {
          kind: "edge_agent",
          executor_id: "edge-1",
          status: "offline",
        },
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("review");
    });

    expect(result.current.runId).toBe("run-edge-1");
    expect(result.current.runStatus).toBe("blocked");
    expect(result.current.waitingFor).toBe("executor_offline");
    expect(result.current.workspace).toMatchObject({
      kind: "edge_workspace",
      cwd: "/Users/test/project",
    });
    expect(result.current.executor).toMatchObject({
      kind: "edge_agent",
      executor_id: "edge-1",
      status: "offline",
    });
    expect(result.current.transport).toBe("edge_ws");
    expect(result.current.fallbackPolicy).toBe("disabled");
  });

  test("tracks run_error as a failed bound run", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "run_error",
        run_id: "run-failed-1",
        message: "executor crashed",
        workspace: {
          kind: "edge_workspace",
          cwd: "/Users/test/project",
        },
        executor: {
          kind: "edge_agent",
          executor_id: "edge-1",
          status: "degraded",
        },
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("review");
    });

    expect(result.current.runId).toBe("run-failed-1");
    expect(result.current.runStatus).toBe("failed");
    expect(result.current.waitingFor).toBeNull();
    expect(result.current.error).toBe("executor crashed");
    expect(result.current.isStreaming).toBe(false);
    expect(result.current.workspace).toMatchObject({
      kind: "edge_workspace",
      cwd: "/Users/test/project",
    });
    expect(result.current.executor).toMatchObject({
      kind: "edge_agent",
      executor_id: "edge-1",
    });
  });

  test("tracks run_interrupted as a paused resumable run", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "run_interrupted",
        run_id: "run-paused-1",
        kind: "budget_exhausted",
        message: "Budget exhausted. You can continue.",
        resumable: true,
        workspace: {
          kind: "server_sandbox",
          cwd: "/tmp/astra-workspaces/run-paused-1",
        },
        executor: {
          kind: "server_local",
          executor_id: "server-runtime",
          status: "online",
        },
        transport: "server_local",
        fallback_policy: "disabled",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("review");
    });

    expect(result.current.runId).toBe("run-paused-1");
    expect(result.current.runStatus).toBe("paused");
    expect(result.current.waitingFor).toBe("user_resume");
    expect(result.current.isStreaming).toBe(false);
    expect(result.current.workspace).toMatchObject({
      kind: "server_sandbox",
      cwd: "/tmp/astra-workspaces/run-paused-1",
    });
    expect(result.current.executor).toMatchObject({
      kind: "server_local",
      executor_id: "server-runtime",
    });
    expect(result.current.transport).toBe("server_local");
    expect(result.current.fallbackPolicy).toBe("disabled");
  });

  test("updates run execution boundary from pause, resume, and terminal events", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "run_paused",
        run_id: "run-life-1",
        workspace: { kind: "server_sandbox", cwd: "/tmp/astra/run-life-1" },
        executor: {
          kind: "server_local",
          executor_id: "server-runtime",
          status: "online",
        },
        transport: "server_local",
        fallback_policy: "disabled",
      } as StreamEvent,
      {
        type: "run_resumed",
        run_id: "run-life-1",
        workspace: { kind: "edge_workspace", cwd: "/repo" },
        executor: {
          kind: "edge_agent",
          executor_id: "edge-1",
          status: "online",
        },
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
      {
        type: "run_finished",
        run_id: "run-life-1",
        status: "completed",
        workspace: { kind: "edge_workspace", cwd: "/repo" },
        executor: {
          kind: "edge_agent",
          executor_id: "edge-1",
          status: "online",
        },
        transport: "edge_ledger",
        fallback_policy: "disabled",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("review");
    });

    expect(result.current.runId).toBe("run-life-1");
    expect(result.current.runStatus).toBe("completed");
    expect(result.current.waitingFor).toBeNull();
    expect(result.current.workspace).toMatchObject({
      kind: "edge_workspace",
      cwd: "/repo",
    });
    expect(result.current.executor).toMatchObject({
      kind: "edge_agent",
      executor_id: "edge-1",
    });
    expect(result.current.transport).toBe("edge_ledger");
    expect(result.current.fallbackPolicy).toBe("disabled");
  });

  test("processes usage event", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "usage",
        prompt_tokens: 100,
        completion_tokens: 50,
        cache_creation_tokens: 10,
        cache_read_tokens: 5,
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    expect(result.current.usage.promptTokens).toBe(100);
    expect(result.current.usage.completionTokens).toBe(50);
    expect(result.current.usage.totalTokens).toBe(150);
    expect(result.current.usage.cacheCreationTokens).toBe(10);
    expect(result.current.usage.cacheReadTokens).toBe(5);
  });

  test("processes error event", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: "error", message: "Server error" } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    expect(result.current.error).toBe("Server error");
  });

  test("run_finished finalizes assistant message", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: "text_delta", content: "done" } as StreamEvent,
      { type: "run_finished", run_id: "r1" } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    const lastMsg = result.current.messages[result.current.messages.length - 1];
    expect(lastMsg.streaming).toBe(false);
    expect(lastMsg.content).toBe("done");
    expect(result.current.isStreaming).toBe(false);
  });

  test("stop() aborts stream and finalizes", () => {
    const { client, streamChatMock } = createMockClient();
    // Don't fire run_finished so we stay in streaming state
    mockStreamEvents(streamChatMock, [
      { type: "text_delta", content: "partial" } as StreamEvent,
      {
        type: "tool_call_start",
        call_id: "tc-running",
        tool: "bash",
        arguments: '{"command":"sleep 30"}',
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    // Still streaming (no run_finished)
    expect(result.current.isStreaming).toBe(true);

    act(() => {
      result.current.stop();
    });

    expect(result.current.isStreaming).toBe(false);
    expect(result.current.connectionState).toBe("idle");
    expect(result.current.toolCalls[0]).toMatchObject({
      callId: "tc-running",
      status: "cancelled",
    });
  });

  test("reset() clears all state", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: "session_info", session_id: "sess-1" } as StreamEvent,
      { type: "text_delta", content: "text" } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    expect(result.current.messages.length).toBeGreaterThan(0);

    act(() => {
      result.current.reset();
    });

    expect(result.current.sessionId).toBeNull();
    expect(result.current.messages).toEqual([]);
    expect(result.current.isStreaming).toBe(false);
    expect(result.current.connectionState).toBe("idle");
  });

  test("agent events are tracked", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "agent_delegated",
        agent_id: "a1",
        role: "coder",
        task: "implement feature",
      } as StreamEvent,
      {
        type: "agent_completed",
        agent_id: "a1",
        status: "completed",
        result: "ok",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    expect(result.current.agentEvents).toHaveLength(2);
    expect(result.current.agentEvents[0].type).toBe("agent_delegated");
    expect(result.current.agentEvents[1].type).toBe("agent_completed");
  });

  test("agent lifecycle unhappy path events are tracked with execution bindings", () => {
    const { client, streamChatMock } = createMockClient();
    const workspace = {
      kind: "edge_workspace",
      display_name: "MacBook Pro",
      cwd: "/Users/test/project",
      authority: "read_write",
      fallback_policy: "disabled",
    };
    const executor = {
      kind: "edge_agent",
      executor_id: "edge-1",
      display_name: "MacBook Pro",
      transport: "edge_ws",
      status: "offline",
    };
    mockStreamEvents(streamChatMock, [
      {
        type: "agent_spawned",
        agent_id: "a1",
        run_id: "child-1",
        parent_run_id: "root-1",
        agent_type: "code-review",
        description: "Review branch",
        workspace,
        executor,
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
      {
        type: "agent_waiting",
        agent_id: "a1",
        run_id: "child-1",
        status: "waiting",
        reason: "executor_offline",
        workspace,
        executor,
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
      {
        type: "agent_failed",
        agent_id: "a1",
        status: "failed",
        error: "executor offline",
        workspace,
        executor,
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
      {
        type: "agent_cancelled",
        agent_id: "a2",
        status: "cancelled",
        reason: "parent stopped",
        workspace,
        executor,
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
      {
        type: "agent_interrupted",
        agent_id: "a3",
        status: "interrupted",
        reason: "stop requested",
        partial_summary: "partial review",
        workspace,
        executor,
        transport: "edge_ws",
        fallback_policy: "disabled",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("review");
    });

    expect(result.current.agentEvents.map((event) => event.type)).toEqual([
      "agent_spawned",
      "agent_waiting",
      "agent_failed",
      "agent_cancelled",
      "agent_interrupted",
    ]);
    expect(result.current.agentEvents[1]).toMatchObject({
      type: "agent_waiting",
      workspace: { kind: "edge_workspace", cwd: "/Users/test/project" },
      executor: { kind: "edge_agent", executor_id: "edge-1" },
      transport: "edge_ws",
      fallback_policy: "disabled",
    });
  });

  test("plan events create and update plan state", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "plan_created",
        plan: {
          plan_id: "p1",
          title: "Test Plan",
          subtasks: [
            { id: "s1", title: "Step 1" },
            { id: "s2", title: "Step 2" },
          ],
        },
      } as unknown as StreamEvent,
      {
        type: "plan_step_start",
        subtask_id: "s1",
        step: "s1",
      } as StreamEvent,
      {
        type: "plan_step_done",
        subtask_id: "s1",
        step: "s1",
        result: "success",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() => useAstraChat({ client, model: "test-model" }));

    act(() => {
      result.current.sendMessage("Hi");
    });

    expect(result.current.plan).not.toBeNull();
    expect(result.current.plan?.title).toBe("Test Plan");
    expect(result.current.plan?.subtasks).toHaveLength(2);
    expect(result.current.plan?.subtasks[0].status).toBe("done");
    expect(result.current.plan?.subtasks[1].status).toBe("pending");
  });

  test("plan_step_done failed aliases mark the step as error", () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: "plan_created",
        plan: {
          plan_id: "p1",
          title: "Test Plan",
          subtasks: [{ id: "s1", title: "Step 1" }],
        },
      } as unknown as StreamEvent,
      {
        type: "plan_step_done",
        subtask_id: "s1",
        step: "s1",
        result: "timed_out",
      } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client, model: "test-model" }),
    );

    act(() => {
      result.current.sendMessage("Hi");
    });

    expect(result.current.plan?.subtasks[0].status).toBe("error");
  });
});

// ─── useAstraRun ───────────────────────────────────────────────────

describe("useAstraRun", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  test("polls run status on mount", async () => {
    const { client, getRunStatusMock, getRunEventsMock } = createMockClient();
    getRunStatusMock.mockResolvedValue({ status: "running" });
    getRunEventsMock.mockResolvedValue([]);

    const { result } = renderHook(() =>
      useAstraRun({ client, runId: "run-1" }),
    );

    // Flush the initial refresh
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(getRunStatusMock).toHaveBeenCalledWith("run-1");
    expect(result.current.status).toBe("running");
  });

  test("accumulates events from polling", async () => {
    const { client, getRunStatusMock, getRunEventsMock } = createMockClient();
    getRunStatusMock.mockResolvedValue({ status: "running" });
    getRunEventsMock
      .mockResolvedValueOnce([{ type: "text_delta", content: "a" }])
      .mockResolvedValueOnce([{ type: "text_delta", content: "b" }])
      .mockResolvedValue([]);

    const { result } = renderHook(() =>
      useAstraRun({ client, runId: "run-1", pollIntervalMs: 1000 }),
    );

    // First poll
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.events).toHaveLength(1);

    // Second poll
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(result.current.events).toHaveLength(2);
  });

  test("sets error on polling failure", async () => {
    const { client, getRunStatusMock, getRunEventsMock } = createMockClient();
    getRunStatusMock.mockRejectedValue(new Error("Network error"));
    getRunEventsMock.mockRejectedValue(new Error("Network error"));

    const { result } = renderHook(() =>
      useAstraRun({ client, runId: "run-1" }),
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(result.current.error).toBe("Network error");
  });
});

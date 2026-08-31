vi.mock("@/lib/api/web-store", () => ({
  setChatActiveRun: vi.fn(),
  updateStreamingAssistantMessage: vi.fn(),
}));

import {
  applyStreamEvent,
  type StreamEventState,
} from "@/lib/api/stream-event-handler";
import {
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";

const mockSetChatActiveRun = vi.mocked(setChatActiveRun);
const mockUpdateStreamingAssistantMessage = vi.mocked(updateStreamingAssistantMessage);

function makeState(): StreamEventState {
  return {
    assistantText: "",
    assistantRawText: "",
    reasoningText: "",
    lastStatus: "streaming",
    protocolError: false,
    runLifecycle: "running",
  };
}

const ctx = {
  ownerUserId: "user-a",
  chatId: "chat-1",
  assistantMessageId: "assistant-1",
  getSessionId: () => "session-1",
};

describe("applyStreamEvent", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("clears active run before throwing on session mismatch", () => {
    const state = makeState();

    expect(() =>
      applyStreamEvent(
        {
          type: "session_info",
          session_id: "wrong-session",
          run_id: "run-1",
        },
        ctx,
        state,
      ),
    ).toThrow("wrong-session");

    expect(mockSetChatActiveRun).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      undefined,
    );
    expect(state.lastStatus).toBe("failed");
    expect(state.runLifecycle).toBe("finished");
  });

  it("does not downgrade completed turns to streaming when turn_complete arrives late", () => {
    const state = makeState();

    applyStreamEvent(
      { type: "run_finished", run_id: "run-1", status: "completed" },
      ctx,
      state,
    );
    applyStreamEvent(
      { type: "turn_complete", assistant_text: "final text" },
      ctx,
      state,
    );

    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "final text",
        status: "complete",
      }),
    );
  });

  it("persists cumulative tool_call_end artifacts on the assistant message", () => {
    const state = makeState();

    applyStreamEvent(
      {
        type: "tool_call_end",
        call_id: "call-1",
        artifacts: [
          {
            artifact_id: "artifact-1",
            name: "report.pdf",
            type: "file",
            data: {
              filename: "report.pdf",
              download_url: "/api/files/file-1",
            },
          },
        ],
      },
      ctx,
      state,
    );
    applyStreamEvent(
      {
        type: "tool_call_end",
        call_id: "call-2",
        artifacts: [
          { artifact_id: "artifact-2", type: "text", data: {} },
        ],
      },
      ctx,
      state,
    );

    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      {
        artifacts: [
          expect.objectContaining({
            id: "artifact-1",
            downloadUrl: "/api/files/file-1",
          }),
          expect.objectContaining({ id: "artifact-2" }),
        ],
      },
    );
  });

  it("does not mark reasoning complete when a run fails", () => {
    const state = makeState();

    applyStreamEvent(
      { type: "reasoning_delta", content: "thinking" },
      ctx,
      state,
    );
    applyStreamEvent(
      {
        type: "run_finished",
        run_id: "run-1",
        status: "failed",
        error: "boom",
      },
      ctx,
      state,
    );

    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "boom",
        reasoning: "thinking",
        reasoningStatus: undefined,
        status: "failed",
      }),
    );
  });

  it("projects run_error as run failure instead of a protocol disconnect", () => {
    const state = makeState();

    applyStreamEvent(
      {
        type: "run_error",
        run_id: "run-1",
        message: "loop crashed",
        error_kind: "runtime",
      },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      undefined,
    );
    expect(state.lastStatus).toBe("failed");
    expect(state.runLifecycle).toBe("finished");
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "loop crashed",
        status: "failed",
      }),
    );
  });

  it("projects blocked run events with explicit messages as visible feedback", () => {
    const state = makeState();

    applyStreamEvent({ type: "run_started", run_id: "run-1" }, ctx, state);
    applyStreamEvent(
      {
        type: "run_blocked",
        session_id: "session-1",
        reason: "transport_disconnected",
        message: "Edge transport disconnected.",
      },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "blocked",
        waitingFor: "transport_disconnected",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "Edge transport disconnected.",
        status: "streaming",
      }),
    );
    expect(state.runLifecycle).toBe("blocked");
  });

  it("projects run_input_queued events into active run state", () => {
    const state = makeState();

    applyStreamEvent({ type: "run_input_queued", run_id: "run-1" }, ctx, state);

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "input-queued",
        waitingFor: "user_input",
        assistantMessageId: "assistant-1",
      }),
    );
    expect(state.runLifecycle).toBe("running");
  });

  it("stores the next stream cursor when projecting active run state", () => {
    const state = makeState();

    applyStreamEvent(
      { type: "run_input_queued", run_id: "run-1", index: 12 },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "input-queued",
        assistantMessageId: "assistant-1",
        nextEventIndex: 13,
      }),
    );
  });

  it("fails closed on malformed event cursors", () => {
    const state = makeState();

    expect(() =>
      applyStreamEvent(
        {
          type: "run_input_queued",
          run_id: "run-1",
          index: "12" as never,
        },
        ctx,
        state,
      ),
    ).toThrow("Malformed stream event index.");
    expect(mockSetChatActiveRun).not.toHaveBeenCalled();
  });

  it("keeps execution-boundary run_waiting events blocked", () => {
    const state = makeState();

    applyStreamEvent({ type: "run_started", run_id: "run-1" }, ctx, state);
    applyStreamEvent(
      {
        type: "run_waiting",
        run_id: "run-1",
        reason: "waiting: executor_offline",
      },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "blocked",
        waitingFor: "executor_offline",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalled();
    expect(state.runLifecycle).toBe("blocked");
  });

  it("keeps unavailable workspace executor waits blocked", () => {
    const state = makeState();

    applyStreamEvent({ type: "run_started", run_id: "run-1" }, ctx, state);
    applyStreamEvent(
      {
        type: "run_waiting",
        run_id: "run-1",
        reason: "waiting: workspace_executor_unavailable",
      },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "blocked",
        waitingFor: "workspace_executor_unavailable",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalled();
    expect(state.runLifecycle).toBe("blocked");
  });

  it("projects generic run_waiting events into waiting state", () => {
    const state = makeState();

    applyStreamEvent({ type: "run_started", run_id: "run-1" }, ctx, state);
    applyStreamEvent(
      {
        type: "run_waiting",
        run_id: "run-1",
        reason: "waiting: tool_approval",
      },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "waiting",
        waitingFor: "tool_approval",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalled();
    expect(state.runLifecycle).toBe("waiting");
  });

  it("keeps blocked fallback state out of assistant prose", () => {
    const state = makeState();

    applyStreamEvent({ type: "run_started", run_id: "run-1" }, ctx, state);
    applyStreamEvent(
      {
        type: "run_blocked",
        run_id: "run-1",
        reason: "fallback_disabled",
      },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "blocked",
        waitingFor: "fallback_disabled",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalled();
  });

  it("renders explicit blocked messages as visible non-terminal feedback", () => {
    const state = makeState();

    applyStreamEvent({ type: "run_started", run_id: "run-1" }, ctx, state);
    applyStreamEvent(
      {
        type: "run_blocked",
        run_id: "run-1",
        reason: "executor_offline",
        message:
          "No alternate execution provider is available for this file environment.",
      },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "blocked",
        waitingFor: "executor_offline",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content:
          "No alternate execution provider is available for this file environment.",
        status: "streaming",
      }),
    );
  });

  it("derives waitingFor from run_blocked reason fields", () => {
    const state = makeState();

    applyStreamEvent({ type: "run_started", run_id: "run-1" }, ctx, state);
    applyStreamEvent(
      {
        type: "run_blocked",
        session_id: "session-1",
        reason: "fallback_disabled",
      },
      ctx,
      state,
    );

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "blocked",
        waitingFor: "fallback_disabled",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalled();
    expect(state.runLifecycle).toBe("blocked");
  });

  it("keeps interrupted runs paused through terminal usage events", () => {
    const state = makeState();

    applyStreamEvent(
      {
        type: "run_interrupted",
        run_id: "run-1",
        kind: "budget_exhausted",
        resumable: true,
        message: "Budget exhausted. You can continue.",
      },
      ctx,
      state,
    );
    applyStreamEvent(
      {
        type: "run_finished",
        run_id: "run-1",
        status: "paused",
        interrupted: true,
        resumable: true,
      },
      ctx,
      state,
    );

    expect(state.lastStatus).toBe("complete");
    expect(state.runLifecycle).toBe("paused");
    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-1",
        status: "paused",
        waitingFor: "user_resume",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "",
        status: "complete",
      }),
    );
  });

  it("preserves task-board intervention waiting state without assistant prose", () => {
    const state = makeState();

    applyStreamEvent(
      {
        type: "run_interrupted",
        run_id: "run-task",
        kind: "empty_completion",
        resumable: true,
        waiting_for: "task_board_intervention",
        message: "Task-board work remains open.",
        task_board: {
          summary: "1 paused task(s) remain: task-2 Investigate [paused]",
          paused_count: 1,
          active_tasks: ["task-2 Investigate [paused]"],
        },
      },
      ctx,
      state,
    );
    applyStreamEvent(
      {
        type: "run_finished",
        run_id: "run-task",
        status: "paused",
        interrupted: true,
        waiting_for: "task_board_intervention",
      },
      ctx,
      state,
    );

    expect(state.runLifecycle).toBe("paused");
    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-task",
        status: "paused",
        waitingFor: "task_board_intervention",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "",
        status: "complete",
      }),
    );
  });

  it("clears reasoning that only came from previous thinking tags", () => {
    const state = makeState();

    applyStreamEvent(
      {
        type: "text_done",
        full_text: "<thinking>hidden</thinking>visible",
      },
      ctx,
      state,
    );
    applyStreamEvent(
      {
        type: "text_done",
        full_text: "visible only",
      },
      ctx,
      state,
    );

    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "visible only",
        reasoning: undefined,
      }),
    );
  });
});

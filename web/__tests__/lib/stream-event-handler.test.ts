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
import type { StreamEvent } from "@astra/sdk";

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

  it("shows report publication failure independently of a successful answer", () => {
    const state = makeState();
    const event = { type: "artifact_publication" as const, schema_version: 1 as const,
      run_id: "run-1", turn_id: "turn-1", execution_owner_generation: 1,
      artifact_type: "explain_analyze_snapshot" as const, recorded: false,
      status: "unavailable" as const, reason_code: "storage_failed", message: "Report storage failed." };
    applyStreamEvent(event, ctx, state);
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a", "chat-1", "assistant-1", { artifactPublication: event });
    expect(state.protocolError).toBe(false);
    expect(mockSetChatActiveRun).not.toHaveBeenCalled();
  });

  it("persists validated Explain Analyze facts and records a stream gap", () => {
    const state = makeState();
    const event = {
      type: "explain_analyze" as const,
      schema_version: 1 as const,
      event_id: "clock-1:1",
      run_id: "run-1",
      turn_id: "turn-1",
      node_id: "turn-1/model/0",
      producer_id: "worker-1",
      clock_domain_id: "clock-1",
      kind: "model_round" as const,
      round_index: 0,
      attempt_index: 0,
      label: "Generate model response",
      transition: "started" as const,
      elapsed_ms: 4,
    };

    applyStreamEvent(event, ctx, state);
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      { explainAnalyzeEvent: event },
    );

    applyStreamEvent(
      {
        type: "stream_gap",
        run_id: "run-1",
        dropped_event_count: 2,
        repair: "refresh_run_snapshot",
      },
      ctx,
      state,
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      { explainAnalyzeDegraded: true },
    );
  });

  it("does not persist a false warning for a fully recovered Explain gap", () => {
    applyStreamEvent({ type: "stream_gap", run_id: "run-1", dropped_event_count: 2,
      explain_analyze_recovered: true, repair: "refresh_run_snapshot" }, ctx, makeState());
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalled();
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

  it("keeps partial output and exposes a terminal persistence failure", () => {
    const state = makeState();
    applyStreamEvent({ type: "session_info", session_id: "session-1", run_id: "run-1" }, ctx, state);
    applyStreamEvent({ type: "text_delta", content: "Useful partial answer" }, ctx, state);
    applyStreamEvent({ type: "reasoning_delta", content: "Partial reasoning" }, ctx, state);
    applyStreamEvent({ type: "run_error", run_id: "run-1",
      error: "Completion could not be saved.", error_code: "canonical_persistence_failed" }, ctx, state);
    expect(state.assistantText).toBe("Useful partial answer\n\nCompletion could not be saved.");
    applyStreamEvent({ type: "run_error", run_id: "run-1",
      error: "Completion could not be saved.", error_code: "canonical_persistence_failed" }, ctx, state);
    expect(state.assistantText).toBe("Useful partial answer\n\nCompletion could not be saved.");
    for (const error of ["Completion could not be saved.", undefined]) {
      applyStreamEvent({ type: "run_finished", run_id: "run-1", status: "failed", error }, ctx, state);
    }
    expect(state.assistantRawText).toBe("Useful partial answer");
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith("user-a", "chat-1", "assistant-1",
      expect.objectContaining({ content: "Useful partial answer\n\nCompletion could not be saved.",
        reasoning: "Partial reasoning", reasoningStatus: undefined, status: "failed" }));
    expect(state.lastStatus).toBe("failed");
  });

  it("keeps root lifecycle stable while child events advance the shared cursor", () => {
    const state = makeState();
    applyStreamEvent({ type: "session_info", session_id: "session-1", run_id: "root" }, ctx, state);
    const children = [
      { type: "run_started" }, { type: "run_input_queued" }, { type: "run_blocked", reason: "offline" },
      { type: "run_waiting" }, { type: "run_paused" }, { type: "run_resumed" }, { type: "run_interrupted" },
      { type: "run_error", error: "child only" },
      ...["completed", "cancelled", "failed"].map(status => ({ type: "run_finished", status })),
    ];
    for (const failed of [false, true]) {
      if (failed) applyStreamEvent({ type: "run_error", run_id: "root", error: "root failure" }, ctx, state);
      vi.clearAllMocks();
      const before = { ...state };
      children.forEach((child, index) => applyStreamEvent(
        { ...child, run_id: "child", index: index + (failed ? 20 : 0) } as StreamEvent, ctx, state));
      expect(state).toEqual({ ...before, nextEventIndex: failed ? 31 : 11 });
      expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalled();
      expect(mockSetChatActiveRun).not.toHaveBeenCalled();
    }
    applyStreamEvent({ type: "run_resumed", run_id: "root" }, ctx, state);
    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith("user-a", "chat-1",
      expect.objectContaining({ runId: "root", nextEventIndex: 31 }));
  });

  it("retains distinct failure reasons even after an earlier generic error", () => {
    const state = makeState();
    applyStreamEvent({ type: "text_delta", content: "Answer mentions save failure" }, ctx, state);
    applyStreamEvent({ type: "error", message: "generic transport issue" }, ctx, state);
    applyStreamEvent({ type: "run_error", run_id: "root", error: "save failure" }, ctx, state);
    applyStreamEvent({ type: "run_finished", run_id: "root", status: "failed", error: "different failure" }, ctx, state);
    applyStreamEvent({ type: "run_finished", run_id: "root", status: "failed", error: "different failure" }, ctx, state);
    expect(state.assistantText).toBe("Answer mentions save failure\n\nsave failure\n\ndifferent failure");
  });

  it("rejects a conflicting root bootstrap without rebinding or accepting later text", () => {
    const state = makeState();
    applyStreamEvent({ type: "session_info", session_id: "session-1", run_id: "root" }, ctx, state);
    expect(() => applyStreamEvent({ type: "session_info", session_id: "session-1", run_id: "other" }, ctx, state))
      .toThrow("root run_id");
    const failure = state.assistantText;
    applyStreamEvent({ type: "text_delta", content: "late" }, ctx, state);
    expect(state.runId).toBe("root");
    expect(state.protocolError).toBe(true);
    expect(state.assistantText).toBe(failure);
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

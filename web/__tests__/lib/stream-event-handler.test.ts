jest.mock("@/lib/api/web-store", () => ({
  setChatActiveRun: jest.fn(),
  updateStreamingAssistantMessage: jest.fn(),
}));

import {
  applyStreamEvent,
  type StreamEventState,
} from "@/lib/api/stream-event-handler";
import {
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";

const mockSetChatActiveRun = setChatActiveRun as jest.MockedFunction<
  typeof setChatActiveRun
>;
const mockUpdateStreamingAssistantMessage =
  updateStreamingAssistantMessage as jest.MockedFunction<
    typeof updateStreamingAssistantMessage
  >;

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
    jest.clearAllMocks();
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

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactNode } from "react";
import { ChatView } from "@/components/app/chat-view";
import { ToastProvider } from "@/components/ui/toast";
import { WebApiError } from "@/lib/api/errors";
import type { ChatDetail, ComposerOptions } from "@/lib/api/types";
import {
  getChat,
  getChatWorkSurface,
  queueChatRunInput,
  resumeChatRun,
  stopChatRun,
  streamChatMessage,
  streamExistingChatRun,
  updateChatModel,
} from "@/lib/api/chats";

const pushMock = jest.fn();
const replaceMock = jest.fn();
const refreshMock = jest.fn();

let composerPayload: {
  text: string;
  options: ComposerOptions;
} = {
  text: "queue this follow-up",
  options: {
    webSearch: false,
    thinking: true,
    model: "sonnet-4.6-adaptive",
    activeSkills: [],
  },
};

jest.mock("next/navigation", () => ({
  useRouter: () => ({
    push: pushMock,
    replace: replaceMock,
    refresh: refreshMock,
  }),
}));

jest.mock("next/link", () => ({
  __esModule: true,
  default: ({ children, href }: { children: ReactNode; href: string }) => (
    <a href={href}>{children}</a>
  ),
}));

jest.mock("lucide-react", () => {
  const Icon = () => null;
  return new Proxy(
    { __esModule: true },
    {
      get: (_target, prop) => (prop === "__esModule" ? true : Icon),
    },
  );
});

jest.mock("@/components/app/chat-actions-menu", () => ({
  ChatActionsMenu: () => null,
}));

jest.mock("@/components/app/chat-dot-navigator", () => ({
  ChatDotNavigator: () => null,
}));

jest.mock("@/components/app/move-chat-modal", () => ({
  MoveChatModal: () => null,
}));

jest.mock("@/components/app/message-bubble", () => ({
  MessageBubble: ({ message }: { message: { content: string } }) => (
    <div>{message.content}</div>
  ),
}));

jest.mock("@/components/ui/icon-button", () => ({
  IconButton: () => null,
}));

jest.mock("@/hooks/use-chat-lifecycle-actions", () => ({
  useChatLifecycleActions: () => ({
    busyChatId: null,
    unarchive: jest.fn(),
  }),
}));

jest.mock("@/lib/chat-lifecycle-events", () => ({
  subscribeChatLifecycleChange: () => () => {},
}));

jest.mock("@/components/app/composer", () => ({
  Composer: ({
    disabled,
    onSubmit,
    showStop,
    stopDisabled,
    onStop,
  }: {
    disabled?: boolean;
    showStop?: boolean;
    stopDisabled?: boolean;
    onStop?: () => void;
    onSubmit: (payload: {
      text: string;
      attachments: [];
      options: ComposerOptions;
    }) => Promise<void>;
  }) => (
    <>
      <button
        type="button"
        disabled={disabled}
        onClick={() =>
          void onSubmit({
            text: composerPayload.text,
            attachments: [],
            options: composerPayload.options,
          })
        }
      >
        Submit composer
      </button>
      {showStop ? (
        <button type="button" disabled={stopDisabled} onClick={onStop}>
          Stop run
        </button>
      ) : null}
    </>
  ),
}));

jest.mock("@/lib/api/chats", () => ({
  getChat: jest.fn(),
  getChatWorkSurface: jest.fn(),
  queueChatRunInput: jest.fn(),
  resumeChatRun: jest.fn(),
  stopChatRun: jest.fn(),
  streamChatMessage: jest.fn(),
  streamExistingChatRun: jest.fn(),
  updateChatModel: jest.fn(),
}));

const mockGetChat = getChat as jest.MockedFunction<typeof getChat>;
const mockGetChatWorkSurface = getChatWorkSurface as jest.MockedFunction<
  typeof getChatWorkSurface
>;
const mockQueueChatRunInput = queueChatRunInput as jest.MockedFunction<
  typeof queueChatRunInput
>;
const mockResumeChatRun = resumeChatRun as jest.MockedFunction<
  typeof resumeChatRun
>;
const mockStopChatRun = stopChatRun as jest.MockedFunction<typeof stopChatRun>;
const mockStreamChatMessage = streamChatMessage as jest.MockedFunction<
  typeof streamChatMessage
>;
const mockStreamExistingChatRun = streamExistingChatRun as jest.MockedFunction<
  typeof streamExistingChatRun
>;
const mockUpdateChatModel = updateChatModel as jest.MockedFunction<
  typeof updateChatModel
>;

const defaultActiveRun: NonNullable<ChatDetail["activeRun"]> = {
  runId: "run-123",
  status: "running",
  waitingFor: null,
};

function makeDetail(
  activeRun: ChatDetail["activeRun"] | null = defaultActiveRun,
): ChatDetail {
  return {
    chat: {
      id: "chat-123",
      title: "Test chat",
      projectId: null,
      createdAt: "2026-06-07T00:00:00.000Z",
      updatedAt: "2026-06-07T00:00:00.000Z",
      archivedAt: null,
      model: "sonnet-4.6-adaptive",
    },
    session: {
      chatId: "chat-123",
      backendSessionId: "chat-123",
      persisted: true,
      messageCount: 0,
    },
    messages: [],
    activeRun: activeRun ?? undefined,
  };
}

describe("ChatView deferred-input unhappy paths", () => {
  beforeEach(() => {
    composerPayload = {
      text: "queue this follow-up",
      options: {
        webSearch: false,
        thinking: true,
        model: "sonnet-4.6-adaptive",
        activeSkills: [],
      },
    };
    pushMock.mockReset();
    replaceMock.mockReset();
    refreshMock.mockReset();
    mockGetChat.mockReset();
    mockGetChatWorkSurface.mockReset();
    mockGetChatWorkSurface.mockResolvedValue({
      sessionId: "chat-123",
      runId: "run-123",
      tasks: [],
      events: [],
      generatedAt: "2026-06-07T00:00:00.000Z",
    });
    mockQueueChatRunInput.mockReset();
    mockResumeChatRun.mockReset();
    mockStopChatRun.mockReset();
    mockStreamChatMessage.mockReset();
    mockStreamExistingChatRun.mockReset();
    mockStreamExistingChatRun.mockResolvedValue("");
    mockUpdateChatModel.mockReset();
    window.alert = jest.fn();
    HTMLElement.prototype.scrollTo = jest.fn();
  });

  it("does not start a fresh stream when queueing fails for a non-conflict error", async () => {
    const user = userEvent.setup();
    mockQueueChatRunInput.mockRejectedValue(
      new WebApiError(500, "runtime temporarily unavailable"),
    );

    render(
      <ToastProvider>
        <ChatView initial={makeDetail()} />
      </ToastProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockQueueChatRunInput).toHaveBeenCalledWith("chat-123", {
        content: "queue this follow-up",
        options: composerPayload.options,
      });
    });
    expect(mockGetChat).not.toHaveBeenCalled();
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(
      screen.getByText("runtime temporarily unavailable"),
    ).toBeInTheDocument();
  });

  it("reconciles pending first-turn placeholders with persisted stream messages", async () => {
    mockStreamChatMessage.mockImplementation(
      async (_chatId, _payload, handlers) => {
        handlers.onLocalMessages?.({
          userMessage: {
            id: "pending-user-1",
            role: "user",
            content: "first message",
            createdAt: "2026-06-07T00:00:00.000Z",
            status: "complete",
          },
          assistantMessage: {
            id: "persisted-assistant-1",
            role: "assistant",
            content: "",
            createdAt: "2026-06-07T00:00:01.000Z",
            reasoning: "",
            reasoningStatus: "streaming",
            status: "streaming",
          },
        });
        handlers.onText?.("first streamed reply");
        handlers.onDone?.("first streamed reply");
        return "first streamed reply";
      },
    );

    render(
      <ChatView
        initial={{
          ...makeDetail(null),
          messages: [
            {
              id: "pending-user-1",
              role: "user",
              content: "first message",
              createdAt: "2026-06-07T00:00:00.000Z",
              status: "complete",
            },
          ],
          pendingTurn: {
            messageId: "pending-user-1",
            content: "first message",
            options: {
              webSearch: false,
              thinking: true,
              model: "sonnet-4.6-adaptive",
              activeSkills: [],
            },
          },
        }}
      />,
    );

    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        "chat-123",
        {
          content: "first message",
          options: {
            webSearch: false,
            thinking: true,
            model: "sonnet-4.6-adaptive",
            activeSkills: [],
          },
          pendingMessageId: "pending-user-1",
        },
        expect.objectContaining({
          onLocalMessages: expect.any(Function),
          onText: expect.any(Function),
          onDone: expect.any(Function),
        }),
      );
    });
    await waitFor(() => {
      expect(screen.getByText("first streamed reply")).toBeInTheDocument();
    });
    expect(screen.getAllByText("first message")).toHaveLength(1);
  });

  it("does not start pending first-turn streams during an immediate effect cleanup", () => {
    jest.useFakeTimers();
    try {
      const { unmount } = render(
        <ChatView
          initial={{
            ...makeDetail(null),
            messages: [
              {
                id: "pending-user-1",
                role: "user",
                content: "first message",
                createdAt: "2026-06-07T00:00:00.000Z",
                status: "complete",
              },
            ],
            pendingTurn: {
              messageId: "pending-user-1",
              content: "first message",
              options: {
                webSearch: false,
                thinking: true,
                model: "sonnet-4.6-adaptive",
                activeSkills: [],
              },
            },
          }}
        />,
      );

      unmount();
      jest.runOnlyPendingTimers();

      expect(mockStreamChatMessage).not.toHaveBeenCalled();
    } finally {
      jest.useRealTimers();
    }
  });

  it("starts a fresh stream after a run completes without a run_finished event", async () => {
    const user = userEvent.setup();
    mockStreamChatMessage
      .mockImplementationOnce(async (_chatId, _payload, handlers) => {
        handlers.onRunStarted?.("run-first");
        handlers.onText?.("first reply");
        handlers.onDone?.("first reply");
        return "first reply";
      })
      .mockImplementationOnce(async (_chatId, _payload, handlers) => {
        handlers.onRunStarted?.("run-second");
        handlers.onText?.("second reply");
        handlers.onDone?.("second reply");
        return "second reply";
      });

    render(<ChatView initial={makeDetail(null)} />);

    composerPayload = {
      text: "first turn",
      options: composerPayload.options,
    };
    await user.click(screen.getByRole("button", { name: "Submit composer" }));
    await waitFor(() => {
      expect(screen.getByText("first reply")).toBeInTheDocument();
    });

    composerPayload = {
      text: "second turn",
      options: composerPayload.options,
    };
    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledTimes(2);
    });
    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
    expect(mockStreamChatMessage).toHaveBeenLastCalledWith(
      "chat-123",
      expect.objectContaining({
        content: "second turn",
      }),
      expect.any(Object),
    );
  });

  it("does not replace follow-up streaming text with stale hydrated transcript", async () => {
    jest.useFakeTimers();
    try {
      let streamSignal: AbortSignal | undefined;
      mockGetChat.mockResolvedValue({
        ...makeDetail(null),
        messages: [
          {
            id: "user-old",
            role: "user",
            content: "old turn",
            createdAt: "2026-06-07T00:00:00.000Z",
            status: "complete",
          },
          {
            id: "assistant-old",
            role: "assistant",
            content: "old reply",
            createdAt: "2026-06-07T00:00:01.000Z",
            status: "complete",
          },
        ],
      });
      mockStreamChatMessage.mockImplementation(
        async (_chatId, _payload, handlers) => {
          streamSignal = handlers.signal;
          handlers.onText?.("live second reply");
          return new Promise<string>(() => {});
        },
      );

      render(
        <ChatView
          initial={{
            ...makeDetail(null),
            messages: [
              {
                id: "user-old",
                role: "user",
                content: "old turn",
                createdAt: "2026-06-07T00:00:00.000Z",
                status: "complete",
              },
              {
                id: "assistant-old",
                role: "assistant",
                content: "old reply",
                createdAt: "2026-06-07T00:00:01.000Z",
                status: "complete",
              },
            ],
          }}
        />,
      );

      composerPayload = {
        text: "second turn",
        options: composerPayload.options,
      };
      fireEvent.click(screen.getByRole("button", { name: "Submit composer" }));

      await waitFor(() => {
        expect(screen.getByText("live second reply")).toBeInTheDocument();
      });
      jest.advanceTimersByTime(3_100);
      await Promise.resolve();

      expect(mockGetChat).not.toHaveBeenCalled();
      expect(streamSignal?.aborted).toBe(false);
      expect(screen.getByText("live second reply")).toBeInTheDocument();
    } finally {
      jest.useRealTimers();
    }
  });

  it("falls back to a fresh stream only after an explicit stale-run conflict", async () => {
    const user = userEvent.setup();
    mockQueueChatRunInput.mockRejectedValue(
      new WebApiError(409, "no active run is available for deferred input"),
    );
    mockGetChat.mockResolvedValue(makeDetail(null));
    mockStreamChatMessage.mockResolvedValue("streamed fallback answer");

    render(<ChatView initial={makeDetail()} />);

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockGetChat).toHaveBeenCalledWith("chat-123");
    });
    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        "chat-123",
        {
          content: "queue this follow-up",
          options: composerPayload.options,
          pendingMessageId: undefined,
        },
        expect.any(Object),
      );
    });
    expect(window.alert).not.toHaveBeenCalled();
  });

  it("shows an explicit stop action instead of pretending queued input interrupts immediately", async () => {
    const user = userEvent.setup();
    mockStopChatRun.mockResolvedValue({
      activeRun: {
        runId: "run-123",
        status: "cancelling",
        waitingFor: null,
      },
    });

    render(<ChatView initial={makeDetail()} />);

    expect(screen.queryByText("Run in progress")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Stop run" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Stop run" }));

    await waitFor(() => {
      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123");
    });
    expect(screen.getByText("Stopping")).toBeInTheDocument();
    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
  });

  it("continues queueing follow-up input while the active run is input-queued", async () => {
    const user = userEvent.setup();
    mockQueueChatRunInput.mockResolvedValue({
      userMessage: {
        id: "queued-user-1",
        role: "user",
        content: "queue this follow-up",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "complete",
      },
      activeRun: {
        runId: "run-123",
        status: "input-queued",
        waitingFor: "user_input",
      },
    });

    render(
      <ChatView
        initial={makeDetail({
          runId: "run-123",
          status: "input-queued",
          waitingFor: "user_input",
        })}
      />,
    );

    expect(
      screen.getByText(/Input queued for next tool call/),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockQueueChatRunInput).toHaveBeenCalledWith("chat-123", {
        content: "queue this follow-up",
        options: composerPayload.options,
      });
    });
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(mockStreamExistingChatRun).toHaveBeenCalledWith(
      "chat-123",
      "run-123",
      expect.objectContaining({
        onText: expect.any(Function),
        onDone: expect.any(Function),
      }),
    );
  });

  it("does not send stop while deferred input queueing is in flight", async () => {
    const user = userEvent.setup();
    let resolveQueue: (
      value: Awaited<ReturnType<typeof queueChatRunInput>>,
    ) => void = () => {};
    mockQueueChatRunInput.mockReturnValue(
      new Promise((resolve) => {
        resolveQueue = resolve;
      }),
    );

    render(<ChatView initial={makeDetail()} />);

    await user.click(screen.getByRole("button", { name: "Submit composer" }));
    await waitFor(() => {
      expect(mockQueueChatRunInput).toHaveBeenCalledWith("chat-123", {
        content: "queue this follow-up",
        options: composerPayload.options,
      });
    });
    expect(screen.getByRole("button", { name: "Stop run" })).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Stop run" }));

    expect(mockStopChatRun).not.toHaveBeenCalled();

    resolveQueue({
      userMessage: {
        id: "queued-user-1",
        role: "user",
        content: "queue this follow-up",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "complete",
      },
      activeRun: {
        runId: "run-123",
        status: "input-queued",
        waitingFor: "user_input",
      },
    });
    await waitFor(() => {
      expect(screen.getByText("queue this follow-up")).toBeInTheDocument();
    });
  });

  it("does not queue deferred input while stop is in flight", async () => {
    const user = userEvent.setup();
    let resolveStop: (
      value: Awaited<ReturnType<typeof stopChatRun>>,
    ) => void = () => {};
    mockStopChatRun.mockReturnValue(
      new Promise((resolve) => {
        resolveStop = resolve;
      }),
    );

    render(<ChatView initial={makeDetail()} />);

    await user.click(screen.getByRole("button", { name: "Stop run" }));
    await waitFor(() => {
      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123");
    });
    expect(
      screen.getByRole("button", { name: "Submit composer" }),
    ).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    expect(mockQueueChatRunInput).not.toHaveBeenCalled();

    resolveStop({
      activeRun: {
        runId: "run-123",
        status: "cancelling",
        waitingFor: null,
      },
    });
    await waitFor(() => {
      expect(screen.getByText("Stopping")).toBeInTheDocument();
    });
  });

  it("does not queue input for terminal active-run statuses", async () => {
    const user = userEvent.setup();
    mockStreamChatMessage.mockResolvedValue("new answer");

    render(
      <ChatView
        initial={makeDetail({
          runId: "run-123",
          status: "completed",
          waitingFor: null,
        })}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        "chat-123",
        {
          content: "queue this follow-up",
          options: composerPayload.options,
          pendingMessageId: undefined,
        },
        expect.objectContaining({
          signal: expect.any(AbortSignal),
        }),
      );
    });
    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
  });

  it("blocks new input for unknown non-terminal active-run statuses", async () => {
    const user = userEvent.setup();

    render(
      <ChatView
        initial={makeDetail({
          runId: "run-123",
          status: "initializing-provider",
          waitingFor: null,
        })}
      />,
    );

    expect(screen.getByText("Run initializing-provider")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Submit composer" }),
    ).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
  });

  it("does not auto-scroll deferred messages over manual scrollback", async () => {
    const user = userEvent.setup();
    const scrollTo = jest.fn();
    mockQueueChatRunInput.mockResolvedValue({
      userMessage: {
        id: "queued-user-1",
        role: "user",
        content: "queue this follow-up",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "complete",
      },
      activeRun: {
        runId: "run-123",
        status: "input-queued",
        waitingFor: "user_input",
      },
    });

    render(
      <ChatView
        initial={{
          ...makeDetail(),
          messages: [
            {
              id: "existing-user",
              role: "user",
              content: "older message",
              createdAt: "2026-06-07T00:00:00.000Z",
              status: "complete",
            },
          ],
        }}
      />,
    );

    const scroller = screen.getByTestId("chat-scroll-container");
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 1000 },
      scrollTop: { configurable: true, value: 200 },
      clientHeight: { configurable: true, value: 500 },
      scrollTo: { configurable: true, value: scrollTo },
    });
    fireEvent.scroll(scroller);
    scrollTo.mockClear();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(screen.getByText("queue this follow-up")).toBeInTheDocument();
    });
    expect(scrollTo).not.toHaveBeenCalled();
  });

  it("auto-scrolls deferred messages when the user is pinned to the bottom", async () => {
    const user = userEvent.setup();
    const scrollTo = jest.fn();
    mockQueueChatRunInput.mockResolvedValue({
      userMessage: {
        id: "queued-user-1",
        role: "user",
        content: "queue this follow-up",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "complete",
      },
      activeRun: {
        runId: "run-123",
        status: "input-queued",
        waitingFor: "user_input",
      },
    });

    render(
      <ChatView
        initial={{
          ...makeDetail(),
          messages: [
            {
              id: "existing-user",
              role: "user",
              content: "older message",
              createdAt: "2026-06-07T00:00:00.000Z",
              status: "complete",
            },
          ],
        }}
      />,
    );

    const scroller = screen.getByTestId("chat-scroll-container");
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 1000 },
      scrollTop: { configurable: true, value: 430 },
      clientHeight: { configurable: true, value: 500 },
      scrollTo: { configurable: true, value: scrollTo },
    });
    fireEvent.scroll(scroller);
    scrollTo.mockClear();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(scrollTo).toHaveBeenCalledWith({ top: 1000 });
    });
  });

  it("lets the web user resume a paused run instead of trapping the composer", async () => {
    const user = userEvent.setup();
    mockResumeChatRun.mockResolvedValue({
      activeRun: {
        runId: "run-123",
        status: "running",
        waitingFor: null,
      },
    });
    mockStreamExistingChatRun.mockResolvedValue("resumed assistant text");

    render(
      <ChatView
        initial={{
          ...makeDetail({
            runId: "run-123",
            status: "paused",
            waitingFor: null,
          }),
          messages: [
            {
              id: "assistant-1",
              role: "assistant",
              content: "Partial reply",
              createdAt: "2026-06-07T00:00:00.000Z",
              status: "streaming",
            },
          ],
        }}
      />,
    );

    expect(
      screen.getByText(
        "This run is paused. Resume to continue or Stop to cancel it.",
      ),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Resume" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Stop" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Resume" }));

    await waitFor(() => {
      expect(mockResumeChatRun).toHaveBeenCalledWith("chat-123");
    });
    await waitFor(() => {
      expect(mockStreamExistingChatRun).toHaveBeenCalledWith(
        "chat-123",
        "run-123",
        expect.objectContaining({
          onRunUpdated: expect.any(Function),
          onDone: expect.any(Function),
          onPaused: expect.any(Function),
        }),
      );
    });
  });

  it("patches the paused streaming assistant instead of the last completed assistant on resume", async () => {
    const user = userEvent.setup();
    mockResumeChatRun.mockResolvedValue({
      activeRun: {
        runId: "run-123",
        status: "running",
        waitingFor: null,
      },
    });
    mockStreamExistingChatRun.mockImplementation(
      async (_chatId, _runId, handlers) => {
        handlers.onText?.("Resumed patch");
        handlers.onDone?.("Resumed final");
        return "Resumed final";
      },
    );

    render(
      <ChatView
        initial={{
          ...makeDetail({
            runId: "run-123",
            status: "paused",
            waitingFor: null,
          }),
          messages: [
            {
              id: "assistant-paused",
              role: "assistant",
              content: "Partial paused reply",
              createdAt: "2026-06-07T00:00:00.000Z",
              status: "streaming",
            },
            {
              id: "assistant-complete",
              role: "assistant",
              content: "Later completed note",
              createdAt: "2026-06-07T00:00:01.000Z",
              status: "complete",
            },
          ],
        }}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Resume" }));

    await waitFor(() => {
      expect(screen.getByText("Resumed final")).toBeInTheDocument();
    });
    expect(screen.getByText("Later completed note")).toBeInTheDocument();
    expect(screen.queryByText("Partial paused reply")).not.toBeInTheDocument();
  });

  it("reconnects a resumed paused run even when no streaming assistant is present", async () => {
    const user = userEvent.setup();
    mockResumeChatRun.mockResolvedValue({
      activeRun: {
        runId: "run-123",
        status: "running",
        waitingFor: null,
      },
    });
    mockStreamExistingChatRun.mockImplementation(
      async (_chatId, _runId, handlers) => {
        handlers.onText?.("Recovered stream text");
        handlers.onDone?.("Recovered stream final");
        return "Recovered stream final";
      },
    );

    render(
      <ChatView
        initial={{
          ...makeDetail({
            runId: "run-123",
            status: "paused",
            waitingFor: null,
          }),
          messages: [
            {
              id: "assistant-complete",
              role: "assistant",
              content: "Previous complete answer",
              createdAt: "2026-06-07T00:00:00.000Z",
              status: "complete",
            },
          ],
        }}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Resume" }));

    await waitFor(() => {
      expect(mockStreamExistingChatRun).toHaveBeenCalledWith(
        "chat-123",
        "run-123",
        expect.objectContaining({
          onText: expect.any(Function),
          onDone: expect.any(Function),
        }),
      );
    });
    await waitFor(() => {
      expect(screen.getByText("Recovered stream final")).toBeInTheDocument();
    });
    expect(screen.getByText("Previous complete answer")).toBeInTheDocument();
  });

  it("refreshes chat detail when a resumed run cannot reconnect to the stream", async () => {
    const user = userEvent.setup();
    mockResumeChatRun.mockResolvedValue({
      activeRun: {
        runId: "run-123",
        status: "running",
        waitingFor: null,
      },
    });
    mockStreamExistingChatRun.mockRejectedValue(
      new Error("stream socket closed"),
    );
    mockGetChat.mockResolvedValue({
      ...makeDetail({
        runId: "run-123",
        status: "paused",
        waitingFor: "user_resume",
      }),
      messages: [
        {
          id: "assistant-refreshed",
          role: "assistant",
          content: "Refreshed paused transcript",
          createdAt: "2026-06-07T00:00:00.000Z",
          status: "streaming",
        },
      ],
    });

    render(
      <ToastProvider>
        <ChatView
          initial={{
            ...makeDetail({
              runId: "run-123",
              status: "paused",
              waitingFor: null,
            }),
            messages: [
              {
                id: "assistant-paused",
                role: "assistant",
                content: "Partial paused reply",
                createdAt: "2026-06-07T00:00:00.000Z",
                status: "streaming",
              },
            ],
          }}
        />
      </ToastProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Resume" }));

    await waitFor(() => {
      expect(mockGetChat).toHaveBeenCalledWith("chat-123");
    });
    await waitFor(() => {
      expect(
        screen.getByText("Refreshed paused transcript"),
      ).toBeInTheDocument();
    });
    expect(
      screen.getByText((content) =>
        content.includes("could not reconnect to its stream"),
      ),
    ).toBeInTheDocument();
  });
});

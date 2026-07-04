import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactNode } from "react";
import { ChatView } from "@/components/app/chat-view";
import { ToastProvider } from "@/components/ui/toast";
import { WebApiError } from "@/lib/api/errors";
import type { ChatDetail, ComposerOptions } from "@/lib/api/types";
import {
  getEdgeStatus,
  getChat,
  getChatWorkSurface,
  getChatWorkSurfaceRun,
  queueChatRunInput,
  resumeChatRun,
  stopChatRun,
  streamChatMessage,
  streamExistingChatRun,
  updateChatModel,
} from "@/lib/api/chats";

const pushMock = vi.fn();
const replaceMock = vi.fn();
const refreshMock = vi.fn();

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

vi.mock("next/navigation", () => ({
  useRouter: () => ({
    push: pushMock,
    replace: replaceMock,
    refresh: refreshMock,
  }),
}));

vi.mock("next/link", () => ({
  __esModule: true,
  default: ({ children, href }: { children: ReactNode; href: string }) => (
    <a href={href}>{children}</a>
  ),
}));

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
    HardDrive: Icon,
    Loader2: Icon,
    MessageSquare: Icon,
    MoreVertical: Icon,
    Monitor: Icon,
    Pause: Icon,
    RefreshCw: Icon,
    RotateCw: Icon,
    Terminal: Icon,
    Wrench: Icon,
    X: Icon,
  };
});

vi.mock("@/components/app/chat-actions-menu", () => ({
  ChatActionsMenu: () => null,
}));

vi.mock("@/components/app/chat-dot-navigator", () => ({
  ChatDotNavigator: () => null,
}));

vi.mock("@/components/app/move-chat-modal", () => ({
  MoveChatModal: () => null,
}));

vi.mock("@/components/app/message-bubble", () => ({
  MessageBubble: ({ message }: { message: { content: string } }) => (
    <div>{message.content}</div>
  ),
}));

vi.mock("@/components/ui/icon-button", () => ({
  IconButton: () => null,
}));

vi.mock("@/hooks/use-chat-lifecycle-actions", () => ({
  useChatLifecycleActions: () => ({
    busyChatId: null,
    unarchive: vi.fn(),
  }),
}));

vi.mock("@/lib/chat-lifecycle-events", () => ({
  subscribeChatLifecycleChange: () => () => {},
}));

vi.mock("@/components/app/composer", () => ({
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

vi.mock("@/lib/api/chats", () => ({
  getEdgeStatus: vi.fn(),
  getChat: vi.fn(),
  getChatWorkSurface: vi.fn(),
  getChatWorkSurfaceRun: vi.fn(),
  queueChatRunInput: vi.fn(),
  resumeChatRun: vi.fn(),
  stopChatRun: vi.fn(),
  streamChatMessage: vi.fn(),
  streamExistingChatRun: vi.fn(),
  updateChatModel: vi.fn(),
}));

const mockGetEdgeStatus = vi.mocked(getEdgeStatus);
const mockGetChat = vi.mocked(getChat);
const mockGetChatWorkSurface = vi.mocked(getChatWorkSurface);
const mockGetChatWorkSurfaceRun = vi.mocked(getChatWorkSurfaceRun);
const mockQueueChatRunInput = vi.mocked(queueChatRunInput);
const mockResumeChatRun = vi.mocked(resumeChatRun);
const mockStopChatRun = vi.mocked(stopChatRun);
const mockStreamChatMessage = vi.mocked(streamChatMessage);
const mockStreamExistingChatRun = vi.mocked(streamExistingChatRun);
const mockUpdateChatModel = vi.mocked(updateChatModel);

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

function mockAnimationFrameQueue() {
  const callbacks = new Map<number, FrameRequestCallback>();
  let nextId = 1;
  const originalRequestAnimationFrame = globalThis.requestAnimationFrame;
  const originalCancelAnimationFrame = globalThis.cancelAnimationFrame;
  globalThis.requestAnimationFrame = vi.fn((callback) => {
    const id = nextId;
    nextId += 1;
    callbacks.set(id, callback);
    return id;
  });
  globalThis.cancelAnimationFrame = vi.fn((id) => {
    callbacks.delete(id);
  });
  return {
    flushNext() {
      const [id, callback] = [...callbacks.entries()][0] ?? [];
      if (id === undefined || callback === undefined) {
        throw new Error("No animation frame is queued.");
      }
      callbacks.delete(id);
      callback(0);
    },
    restore() {
      globalThis.requestAnimationFrame = originalRequestAnimationFrame;
      globalThis.cancelAnimationFrame = originalCancelAnimationFrame;
    },
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
    mockGetEdgeStatus.mockReset();
    mockGetEdgeStatus.mockResolvedValue({ edges: [] });
    mockGetChat.mockReset();
    mockGetChat.mockResolvedValue(makeDetail(null));
    mockGetChatWorkSurface.mockReset();
    mockGetChatWorkSurface.mockImplementation(() => new Promise(() => {}));
    mockGetChatWorkSurfaceRun.mockReset();
    mockGetChatWorkSurfaceRun.mockResolvedValue({
      runId: "child-run",
      sessionId: "chat-123",
      status: "running",
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
    window.localStorage.clear();
    window.alert = vi.fn();
    HTMLElement.prototype.scrollTo = vi.fn();
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
      expect(mockQueueChatRunInput).toHaveBeenCalledWith(
        "chat-123",
        expect.objectContaining({
          content: "queue this follow-up",
          options: composerPayload.options,
          pendingMessageId: expect.any(String),
        }),
      );
    });
    expect(mockGetChat).not.toHaveBeenCalled();
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(
      screen.getByText("runtime temporarily unavailable"),
    ).toBeInTheDocument();
  });

  it("keeps environment controls hidden while an active run can still queue input", () => {
    mockGetChatWorkSurface.mockImplementation(() => new Promise(() => {}));
    mockGetEdgeStatus.mockImplementation(() => new Promise(() => {}));

    render(<ChatView initial={makeDetail()} />);

    expect(screen.queryByRole("button", { name: "Sandbox" })).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Submit composer" }),
    ).not.toBeDisabled();
  });

  it("shows workspace selection errors directly in the assistant message", async () => {
    const user = userEvent.setup();
    const message =
      "The referenced path is outside the selected file environment: /workspace/other. Choose the environment that contains it or use a path inside the current one.";
    composerPayload = {
      text: "review /workspace/other",
      options: composerPayload.options,
    };
    mockStreamChatMessage.mockRejectedValue(
      new WebApiError(409, message, "workspace_path_mismatch"),
    );

    render(<ChatView initial={makeDetail(null)} />);

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(screen.getByText(message)).toBeInTheDocument();
    });
    expect(
      screen.queryByText(/could not reach the Astra runtime/i),
    ).not.toBeInTheDocument();
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
    vi.useFakeTimers();
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
      vi.runOnlyPendingTimers();

      expect(mockStreamChatMessage).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
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

  it("sends the chat-scoped selected edge workspace with new turns", async () => {
    const user = userEvent.setup();
    window.localStorage.setItem(
      "astra.web.workspaceSelection.chat-123",
      JSON.stringify({
        kind: "edge_workspace",
        edgeAgentId: "edge-1",
        displayName: "MacBook Pro",
        cwd: "/Users/test/astra",
      }),
    );
    mockGetEdgeStatus.mockResolvedValue({
      edges: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 10,
        },
      ],
    });
    mockStreamChatMessage.mockResolvedValue("edge answer");

    render(<ChatView initial={makeDetail(null)} />);

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        "chat-123",
        expect.objectContaining({
          workspace: {
            kind: "edge_workspace",
            edgeAgentId: "edge-1",
            displayName: "MacBook Pro",
            cwd: "/Users/test/astra",
          },
        }),
        expect.any(Object),
      );
    });
  });

  it("does not expose environment controls in the main composer", async () => {
    const user = userEvent.setup();
    mockGetEdgeStatus.mockRejectedValue(new WebApiError(404, "Not Found"));
    mockStreamChatMessage.mockResolvedValue("default answer");

    render(<ChatView initial={makeDetail(null)} />);

    expect(screen.queryByText("Run in")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Sandbox" })).not.toBeInTheDocument();
    expect(screen.queryByText("404 Not Found")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        "chat-123",
        expect.not.objectContaining({ workspace: expect.anything() }),
        expect.any(Object),
      );
    });
  });

  it("uses the persisted chat workspace selection for new turns", async () => {
    const user = userEvent.setup();
    const edgeWorkspace = {
      kind: "edge_workspace" as const,
      edgeAgentId: "edge-1",
      displayName: "MacBook Pro",
      cwd: "/Users/test/astra",
    };
    mockGetEdgeStatus.mockResolvedValue({
      edges: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 10,
        },
      ],
    });
    mockStreamChatMessage.mockResolvedValue("edge answer");

    render(
      <ChatView
        initial={{
          ...makeDetail(null),
          workspaceSelection: edgeWorkspace,
        }}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        "chat-123",
        expect.objectContaining({
          workspace: edgeWorkspace,
        }),
        expect.any(Object),
      );
    });
  });

  it("does not replace follow-up streaming text with stale hydrated transcript", async () => {
    vi.useFakeTimers();
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
      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "Submit composer" }));
        vi.advanceTimersByTime(16);
        await Promise.resolve();
      });
      expect(screen.getByText("live second reply")).toBeInTheDocument();
      await act(async () => {
        vi.advanceTimersByTime(3_100);
        await Promise.resolve();
      });

      expect(mockGetChat).not.toHaveBeenCalled();
      expect(streamSignal?.aborted).toBe(false);
      expect(screen.getByText("live second reply")).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
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
    mockStopChatRun.mockResolvedValue({});

    render(<ChatView initial={makeDetail()} />);

    expect(screen.queryByText("Run in progress")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Stop run" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Stop run" }));

    await waitFor(() => {
      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123");
    });
    expect(screen.getByText("Stopped.")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Stop run" }),
    ).not.toBeInTheDocument();
    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
  });

  it("keeps activity details available without main-chat metric links", async () => {
    const user = userEvent.setup();
    mockGetChatWorkSurface.mockResolvedValue({
      sessionId: "chat-123",
      runId: "run-123",
      tasks: [
        {
          id: "task-1",
          title: "Review branch",
          status: "pending",
          created_at: "2026-06-07T00:00:00.000Z",
          updated_at: "2026-06-07T00:00:00.000Z",
        },
      ],
      events: [
        {
          type: "agent_spawned",
          agent_id: "agent-1",
          run_id: "child-run",
          parent_run_id: "run-123",
          agent_type: "code-review",
          description: "Review the branch",
          timestamp: 1_801_000_000_000,
        },
        {
          type: "agent_completed",
          agent_id: "agent-1",
          result_summary: "No blockers",
          timestamp: 1_801_000_001_000,
        },
        {
          type: "tool_transport_failed",
          call_id: "call-1",
          tool: "bash",
          success: false,
          error: "Edge transport disconnected",
          error_kind: "transport_disconnected",
          blocked: true,
          workspace: {
            kind: "edge_workspace",
            display_name: "MacBook Pro",
            cwd: "/Users/test/astra",
            authority: "read_write",
            fallback_policy: "disabled",
          },
          executor: {
            kind: "edge_agent",
            executor_id: "edge-macbook-1",
            display_name: "MacBook Pro",
            transport: "edge_ws",
            status: "offline",
          },
          transport: "edge_ws",
          fallback_policy: "disabled",
          route: "edge_agent",
          timestamp: 1_801_000_002_000,
        },
      ],
      generatedAt: "2026-06-07T00:00:00.000Z",
    });

    render(<ChatView initial={makeDetail()} />);

    expect(
      screen.queryByRole("button", { name: /Open agents activity/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /Open tasks activity/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /Open tools activity/i }),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Tools" }));
    expect(await screen.findAllByText("Needs attention")).not.toHaveLength(0);
    expect(
      await screen.findAllByText("Environment unavailable"),
    ).not.toHaveLength(0);
    expect(screen.getAllByText("Runtime").length).toBeGreaterThan(0);
    expect(screen.getAllByText("MacBook Pro").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Files").length).toBeGreaterThan(0);
    expect(
      screen.getAllByText("/Users/test/astra").length,
    ).toBeGreaterThan(0);
    expect(screen.getAllByText("Connection").length).toBeGreaterThan(0);
    expect(screen.getAllByText("edge ws").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Policy").length).toBeGreaterThan(0);
    expect(screen.getAllByText("disabled").length).toBeGreaterThan(0);
    expect(
      screen.getAllByText(
        "Execution connection disconnected. Reconnect it before retrying.",
      ).length,
    ).toBeGreaterThan(0);

    await user.click(screen.getByRole("button", { name: /Agents/ }));
    expect(await screen.findAllByText("No blockers")).not.toHaveLength(0);
    expect(await screen.findAllByText("Live activity")).not.toHaveLength(0);
    await waitFor(() => {
      expect(mockGetChatWorkSurfaceRun).toHaveBeenCalledWith(
        "chat-123",
        "child-run",
      );
    });
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
      assistantMessage: {
        id: "queued-assistant-1",
        role: "assistant",
        content: "",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "streaming",
      },
      activeRun: {
        runId: "run-123",
        status: "input-queued",
        waitingFor: "user_input",
        assistantMessageId: "queued-assistant-1",
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

    expect(screen.getAllByText("Message queued").length).toBeGreaterThan(0);

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockQueueChatRunInput).toHaveBeenCalledWith(
        "chat-123",
        expect.objectContaining({
          content: "queue this follow-up",
          options: composerPayload.options,
          pendingMessageId: expect.any(String),
        }),
      );
    });
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(mockStreamExistingChatRun).toHaveBeenCalledWith(
      "chat-123",
      "run-123",
      expect.objectContaining({
        onText: expect.any(Function),
        onDone: expect.any(Function),
      }),
      expect.objectContaining({
        assistantMessageId: "queued-assistant-1",
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
      expect(mockQueueChatRunInput).toHaveBeenCalledWith(
        "chat-123",
        expect.objectContaining({
          content: "queue this follow-up",
          options: composerPayload.options,
          pendingMessageId: expect.any(String),
        }),
      );
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
      assistantMessage: {
        id: "queued-assistant-1",
        role: "assistant",
        content: "",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "streaming",
      },
      activeRun: {
        runId: "run-123",
        status: "input-queued",
        waitingFor: "user_input",
        assistantMessageId: "queued-assistant-1",
      },
    });
    await waitFor(() => {
      expect(screen.getByText("queue this follow-up")).toBeInTheDocument();
    });
  });

  it("keeps the visible run cancelling while backend cancellation is in flight", async () => {
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
    expect(screen.getByText("Stopped.")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Submit composer" }),
    ).toBeDisabled();
    expect(screen.getAllByText("Stopping").length).toBeGreaterThan(0);

    resolveStop({});
    await waitFor(() => {
      expect(screen.queryByText("Stopping")).not.toBeInTheDocument();
    });
    expect(
      screen.getByRole("button", { name: "Submit composer" }),
    ).not.toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

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
    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
  });

  it("restores the active run when stop and refresh both fail", async () => {
    const user = userEvent.setup();
    mockStopChatRun.mockRejectedValue(new Error("runtime cancellation failed"));
    mockGetChat.mockRejectedValue(new Error("refresh failed"));

    render(
      <ToastProvider>
        <ChatView
          initial={{
            ...makeDetail({
              ...defaultActiveRun,
              assistantMessageId: "assistant-active",
            }),
            messages: [
              {
                id: "assistant-active",
                role: "assistant",
                content: "working...",
                createdAt: "2026-06-07T00:00:00.000Z",
                status: "streaming",
                reasoningStatus: "streaming",
              },
            ],
          }}
        />
      </ToastProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Stop run" }));

    await waitFor(() => {
      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123");
    });
    await waitFor(() => {
      expect(mockGetChat).toHaveBeenCalledWith("chat-123");
    });
    await waitFor(() => {
      expect(screen.queryByText("Stopping")).not.toBeInTheDocument();
    });
    expect(screen.getAllByText("Thinking").length).toBeGreaterThan(0);
    expect(screen.getByText("working...")).toBeInTheDocument();
    expect(screen.queryByText(/Stopped\./)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Stop run" })).not.toBeDisabled();
    expect(screen.getByText("runtime cancellation failed")).toBeInTheDocument();
  });

  it("restores the active run when stop hangs and refresh fails", async () => {
    vi.useFakeTimers();
    try {
      mockStopChatRun.mockReturnValue(new Promise(() => {}));
      mockGetChat.mockRejectedValue(new Error("refresh failed"));

      render(
        <ToastProvider>
          <ChatView
            initial={{
              ...makeDetail({
                ...defaultActiveRun,
                assistantMessageId: "assistant-active",
              }),
              messages: [
                {
                  id: "assistant-active",
                  role: "assistant",
                  content: "working...",
                  createdAt: "2026-06-07T00:00:00.000Z",
                  status: "streaming",
                  reasoningStatus: "streaming",
                },
              ],
            }}
          />
        </ToastProvider>,
      );

      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "Stop run" }));
        await Promise.resolve();
      });

      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123");
      await act(async () => {
        vi.advanceTimersByTime(10_000);
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(mockGetChat).toHaveBeenCalledWith("chat-123");
      expect(screen.queryByText("Stopping")).not.toBeInTheDocument();
      expect(screen.getAllByText("Thinking").length).toBeGreaterThan(0);
      expect(screen.getByText("working...")).toBeInTheDocument();
      expect(screen.queryByText(/Stopped\./)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Stop run" })).not.toBeDisabled();
      expect(screen.getByText("Stop request timed out.")).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("retries active run stream reattach after a transient failure", async () => {
    vi.useFakeTimers();
    try {
      mockStreamExistingChatRun
        .mockRejectedValueOnce(new Error("temporary stream failure"))
        .mockResolvedValueOnce("");

      render(<ChatView initial={makeDetail()} />);

      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(1);

      await act(async () => {
        vi.advanceTimersByTime(1_000);
        await Promise.resolve();
        await Promise.resolve();
      });

      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
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

    expect(screen.queryByText("Completed")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", {
        name: /Open agents activity/i,
      }),
    ).not.toBeInTheDocument();

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

    expect(screen.getByText("Initializing provider")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Submit composer" }),
    ).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
  });

  it("keeps blocked active runs visible in activity and reattaches", async () => {
    render(
      <ChatView
        initial={makeDetail({
          runId: "run-blocked",
          status: "blocked",
          waitingFor: "executor_offline",
        })}
      />,
    );

    expect(screen.getByText("Needs attention")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Submit composer" }),
    ).toBeEnabled();
    await waitFor(() => {
      expect(mockStreamExistingChatRun).toHaveBeenCalledWith(
        "chat-123",
        "run-blocked",
        expect.any(Object),
        expect.any(Object),
      );
    });
  });

  it("does not auto-scroll deferred messages over manual scrollback", async () => {
    const user = userEvent.setup();
    const scrollTo = vi.fn();
    mockQueueChatRunInput.mockResolvedValue({
      userMessage: {
        id: "queued-user-1",
        role: "user",
        content: "queue this follow-up",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "complete",
      },
      assistantMessage: {
        id: "queued-assistant-1",
        role: "assistant",
        content: "",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "streaming",
      },
      activeRun: {
        runId: "run-123",
        status: "input-queued",
        waitingFor: "user_input",
        assistantMessageId: "queued-assistant-1",
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
    const scrollTo = vi.fn();
    mockQueueChatRunInput.mockResolvedValue({
      userMessage: {
        id: "queued-user-1",
        role: "user",
        content: "queue this follow-up",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "complete",
      },
      assistantMessage: {
        id: "queued-assistant-1",
        role: "assistant",
        content: "",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "streaming",
      },
      activeRun: {
        runId: "run-123",
        status: "input-queued",
        waitingFor: "user_input",
        assistantMessageId: "queued-assistant-1",
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

  it("does not auto-scroll streaming updates over upward scroll intent", async () => {
    const scrollTo = vi.fn();
    let streamHandlers: Parameters<typeof streamExistingChatRun>[2] | undefined;
    mockStreamExistingChatRun.mockImplementation(
      (_chatId, _runId, handlers) => {
        streamHandlers = handlers;
        return new Promise<string>(() => {});
      },
    );

    render(
      <ChatView
        initial={{
          ...makeDetail({
            runId: "run-123",
            status: "running",
            waitingFor: null,
            assistantMessageId: "assistant-streaming",
          }),
          messages: [
            {
              id: "user-1",
              role: "user",
              content: "older question",
              createdAt: "2026-06-07T00:00:00.000Z",
              status: "complete",
            },
            {
              id: "assistant-streaming",
              role: "assistant",
              content: "",
              reasoning: "Initial reasoning",
              reasoningStatus: "streaming",
              createdAt: "2026-06-07T00:00:01.000Z",
              status: "streaming",
            },
          ],
        }}
      />,
    );

    await waitFor(() => {
      expect(streamHandlers).toBeDefined();
    });
    expect(mockStreamExistingChatRun).toHaveBeenCalledWith(
      "chat-123",
      "run-123",
      expect.any(Object),
      expect.objectContaining({ assistantMessageId: "assistant-streaming" }),
    );

    const scroller = screen.getByTestId("chat-scroll-container");
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2000 },
      scrollTop: { configurable: true, value: 1500 },
      clientHeight: { configurable: true, value: 500 },
      scrollTo: { configurable: true, value: scrollTo },
    });
    fireEvent.wheel(scroller, { deltaY: -120 });
    scrollTo.mockClear();

    const animationFrames = mockAnimationFrameQueue();
    try {
      act(() => {
        streamHandlers?.onText?.("Streaming text update");
        animationFrames.flushNext();
      });
    } finally {
      animationFrames.restore();
    }

    expect(scrollTo).not.toHaveBeenCalled();
  });

  it("continues auto-scrolling streaming updates while pinned to the bottom", async () => {
    const scrollTo = vi.fn();
    let streamHandlers: Parameters<typeof streamExistingChatRun>[2] | undefined;
    mockStreamExistingChatRun.mockImplementation(
      (_chatId, _runId, handlers) => {
        streamHandlers = handlers;
        return new Promise<string>(() => {});
      },
    );

    render(
      <ChatView
        initial={{
          ...makeDetail({
            runId: "run-123",
            status: "running",
            waitingFor: null,
            assistantMessageId: "assistant-streaming",
          }),
          messages: [
            {
              id: "assistant-streaming",
              role: "assistant",
              content: "",
              reasoning: "Initial reasoning",
              reasoningStatus: "streaming",
              createdAt: "2026-06-07T00:00:01.000Z",
              status: "streaming",
            },
          ],
        }}
      />,
    );

    await waitFor(() => {
      expect(streamHandlers).toBeDefined();
    });
    expect(mockStreamExistingChatRun).toHaveBeenCalledWith(
      "chat-123",
      "run-123",
      expect.any(Object),
      expect.objectContaining({ assistantMessageId: "assistant-streaming" }),
    );

    const scroller = screen.getByTestId("chat-scroll-container");
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2000 },
      scrollTop: { configurable: true, value: 1500 },
      clientHeight: { configurable: true, value: 500 },
      scrollTo: { configurable: true, value: scrollTo },
    });
    fireEvent.scroll(scroller);
    scrollTo.mockClear();

    const animationFrames = mockAnimationFrameQueue();
    try {
      act(() => {
        streamHandlers?.onText?.("Streaming text update");
        animationFrames.flushNext();
      });
    } finally {
      animationFrames.restore();
    }

    await waitFor(() => {
      expect(screen.getByText("Streaming text update")).toBeInTheDocument();
    });

    await waitFor(() => {
      expect(scrollTo).toHaveBeenCalledWith({ top: 2000 });
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
        "Astra is paused. Resume to continue or stop this run.",
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
        expect.objectContaining({
          assistantMessageId: expect.any(String),
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
        expect.objectContaining({
          assistantMessageId: expect.any(String),
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

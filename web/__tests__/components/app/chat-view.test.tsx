import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { StrictMode, type ReactNode } from "react";
import type { WorkTaskGraphPageV2 } from "@astra/sdk";
import { emitChatLifecycleChange } from "@/lib/chat-lifecycle-events";
import { ChatView } from "@/components/app/chat-view";
import { ToastProvider } from "@/components/ui/toast";
import { WebApiError } from "@/lib/api/errors";
import type { ChatDetail, ComposerOptions } from "@/lib/api/types";
import {
  getEdgeStatus,
  getChat,
  getChatInsights,
  getChatWorkSurface,
  getChatWorkSurfaceRun,
  queueChatRunInput,
  resumeChatRun,
  stopChatRun,
  streamChatMessage,
  streamExistingChatRun,
  updateChatModel,
  updateChatWorkspaceSelection,
} from "@/lib/api/chats";

let composerSubmissionError: unknown;
const pushMock = vi.fn();
const replaceMock = vi.fn();
const refreshMock = vi.fn();

function canonicalTaskGraph(): WorkTaskGraphPageV2 {
  return {
    schema_version: 2,
    scope: "declared_work",
    basis: {
      work_id: "work-1", work_revision: 1, goal_revision: 1, goal: "Review branch",
      criteria_set_revision: 1, criteria_member_count: 0,
      criteria_manifest_hash: `sha256:${"a".repeat(64)}`,
      branch_id: "branch-1", branch_revision: 1, branch_goal_revision: 1,
      branch_criteria_set_revision: 1, branch_basis_graph_revision: 1,
      graph_revision: 1, graph_item_count: 1, graph_edge_count: 0,
      graph_manifest_hash: `sha256:${"b".repeat(64)}`,
    },
    cursor: { graph_revision: 1, item_offset: 0, dependency_offset: 0 },
    next_cursor: null,
    items: {
      offset: 0, limit: 8, total: 1,
      entries: [{
        item_id: "root", revision: 1, kind: "milestone",
        objective: "Review branch", expected_result: "A reviewable branch",
        declaration_state: "active",
        execution: { status: "not_started", terminal: false, run: null },
        delivery: { status: "unreported", summary: null, blocker_kind: null, unavailable_capabilities: [] },
        verification: { status: "unknown", latest_check: null },
      }],
    },
    dependencies: { offset: 0, limit: 128, total: 0, entries: [] },
  };
}

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
    Sparkles: Icon,
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
  MessageBubble: ({ message }: { message: { content: string; reasoning?: string } }) => (
    <div data-reasoning={message.reasoning ?? ""}>{message.content}</div>
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
          }).catch(error => { composerSubmissionError = error; })
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
  getChatInsights: vi.fn(),
  getChatWorkSurface: vi.fn(),
  getChatWorkSurfaceRun: vi.fn(),
  queueChatRunInput: vi.fn(),
  resumeChatRun: vi.fn(),
  stopChatRun: vi.fn(),
  streamChatMessage: vi.fn(),
  streamExistingChatRun: vi.fn(),
  updateChatModel: vi.fn(),
  updateChatWorkspaceSelection: vi.fn(),
}));

const mockGetEdgeStatus = vi.mocked(getEdgeStatus);
const mockGetChat = vi.mocked(getChat);
const mockGetChatInsights = vi.mocked(getChatInsights);
const mockGetChatWorkSurface = vi.mocked(getChatWorkSurface);
const mockGetChatWorkSurfaceRun = vi.mocked(getChatWorkSurfaceRun);
const mockQueueChatRunInput = vi.mocked(queueChatRunInput);
const mockResumeChatRun = vi.mocked(resumeChatRun);
const mockStopChatRun = vi.mocked(stopChatRun);
const mockStreamChatMessage = vi.mocked(streamChatMessage);
const mockStreamExistingChatRun = vi.mocked(streamExistingChatRun);
const mockUpdateChatModel = vi.mocked(updateChatModel);
const mockUpdateChatWorkspaceSelection = vi.mocked(updateChatWorkspaceSelection);

const defaultActiveRun: NonNullable<ChatDetail["activeRun"]> = {
  runId: "run-123",
  status: "running",
  waitingFor: null,
  assistantMessageId: "assistant-live",
  nextEventIndex: 0,
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
    messages: activeRun?.assistantMessageId ? [{
      id: activeRun.assistantMessageId, role: "assistant", content: "",
      createdAt: "2026-06-07T00:00:00.000Z", status: "streaming",
    }] : [],
    activeRun: activeRun ? { nextEventIndex: 0, ...activeRun } : undefined,
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
    flushAll() {
      for (const [id, callback] of [...callbacks]) {
        callbacks.delete(id);
        callback(0);
      }
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
    composerSubmissionError = undefined;
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
    mockUpdateChatWorkspaceSelection.mockReset();
    mockUpdateChatWorkspaceSelection.mockImplementation(
      async (_chatId, workspaceSelection) => ({
        ...makeDetail(null),
        workspaceSelection: workspaceSelection ?? undefined,
        workspaceSelectionExplicit: Boolean(workspaceSelection),
      }),
    );
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
    expect(composerSubmissionError).toBeInstanceOf(WebApiError);
    expect(
      screen.getByText(/runtime temporarily unavailable/),
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

  it("shows hook configuration warnings while preserving the streamed reply", async () => {
    const user = userEvent.setup();
    const message = "Hook configuration .astra/hooks.json at hooks[0].event: invalid event";
    let handlers: Parameters<typeof streamChatMessage>[2] | undefined;
    mockStreamChatMessage.mockImplementation((_chatId, _payload, nextHandlers) => {
      handlers = nextHandlers;
      return new Promise(() => {});
    });
    render(<ToastProvider><ChatView initial={makeDetail(null)} /></ToastProvider>);
    await user.click(screen.getByRole("button", { name: "Submit composer" }));
    act(() => {
      handlers?.onWarning?.(message);
      handlers?.onText?.("answer after warning");
    });
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent(message));
    expect(await screen.findByText("answer after warning")).toBeInTheDocument();
    expect(mockStreamChatMessage).toHaveBeenCalledOnce();
  });

  it.each([
    { mode: "send", clear: "none" }, { mode: "attach", clear: "none" },
    { mode: "attach", clear: "stream" }, { mode: "attach", clear: "done" },
  ])("preserves pending answer patches on canonical failure ($mode, reasoning clear=$clear)", async ({ mode, clear }) => {
    const frames = mockAnimationFrameQueue();
    const reason = "Completion could not be saved. Reopen the session to check its recorded state before retrying.";
    const fail = async (handlers: Parameters<typeof streamChatMessage>[2]) => {
      handlers.onRunStarted?.("run-123");
      handlers.onText?.("Useful partial answer");
      handlers.onReasoning?.("Partial reasoning");
      if (clear === "stream") handlers.onReasoning?.("");
      if (clear === "done") handlers.onReasoningDone?.("");
      handlers.onRunUpdated?.({ runId: "run-123", status: "failed", waitingFor: null });
      throw new WebApiError(500, reason, "canonical_persistence_failed");
    };
    mockStreamChatMessage.mockImplementation((_id, _payload, handlers) => fail(handlers));
    mockStreamExistingChatRun.mockImplementation((_id, _run, handlers) => fail(handlers));
    try {
      render(<ChatView initial={makeDetail(mode === "attach" ? defaultActiveRun : null)} />);
      if (mode === "send") {
        const user = userEvent.setup();
        await user.click(screen.getByRole("button", { name: "Submit composer" }));
      }
      const expected = `Useful partial answer\n\n${reason}`;
      const message = await screen.findByText(expected, { normalizer: text => text });
      expect(message).toHaveAttribute("data-reasoning", clear === "none" ? "Partial reasoning" : "");
      expect(screen.queryByText(/could not reach the Astra runtime/)).not.toBeInTheDocument();
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(mode === "attach" ? 1 : 0);
      expect(mockGetChat).not.toHaveBeenCalled();
    } finally {
      frames.restore();
    }
  });

  it.each(["stream_projection_busy", "stream_continuation_unavailable"])("preserves the answer and active run on %s", async code => {
    const reason = "The observer cannot resume this stream.";
    mockStreamExistingChatRun.mockRejectedValue(new WebApiError(409, reason, code));
    const initial = makeDetail({ ...defaultActiveRun, assistantMessageId: "assistant" });
    initial.messages = [{ id: "assistant", role: "assistant", content: "Saved partial answer",
      reasoning: "Saved reasoning", status: "streaming", createdAt: "2026-06-07T00:00:00Z" }];
    render(<ToastProvider><ChatView initial={initial} /></ToastProvider>);
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent(reason));
    expect(screen.getByText("Saved partial answer")).toBeInTheDocument();
    expect(screen.queryByText(/could not reach the Astra runtime/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
    expect(mockStreamExistingChatRun).toHaveBeenCalledOnce();
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

  it("does not auto-select the only connected edge workspace for new turns", async () => {
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

    render(<ChatView initial={makeDetail(null)} />);

    await waitFor(() => expect(mockGetEdgeStatus).toHaveBeenCalled());
    expect(mockUpdateChatWorkspaceSelection).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Submit composer" }));

    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        "chat-123",
        expect.not.objectContaining({ workspace: edgeWorkspace }),
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

  it("refreshes a queue conflict and requires explicit submission for a fresh run", async () => {
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
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(screen.queryByText("Message queued")).not.toBeInTheDocument();
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
    expect(window.alert).not.toHaveBeenCalled();
  });

  it.each(["committed", "batched", "session", "message"] as const)("ignores stale queue-conflict recovery (%s)", async scope => {
    const batched = scope === "batched";
    const user = userEvent.setup();
    let refresh!: (value: ChatDetail) => void;
    mockQueueChatRunInput.mockRejectedValue(new WebApiError(409, "no active run"));
    mockGetChat.mockImplementation(() => new Promise(resolve => { refresh = resolve; }));
    mockStreamExistingChatRun.mockImplementation(() => new Promise(() => {}));
    render(<ChatView initial={makeDetail()} />);
    await user.click(screen.getByRole("button", { name: "Submit composer" }));
    await waitFor(() => expect(mockGetChat).toHaveBeenCalledOnce());
    const next = makeDetail({ runId: "root-b", status: "running", assistantMessageId: "assistant-b", nextEventIndex: 12 });
    if (scope === "session") {
      next.activeRun = { ...makeDetail().activeRun!, nextEventIndex: 12 };
      next.session = { ...next.session!, backendSessionId: "session-b" };
      next.messages = makeDetail().messages;
    }
    if (scope === "message") next.activeRun!.runId = "run-123";
    next.messages[0].content = "Selected queue replacement B";
    const replace = () => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: next });
    if (!batched) act(replace);
    await act(async () => {
      if (batched) replace();
      refresh(makeDetail(null));
    });
    expect(screen.getByText("Selected queue replacement B")).toBeInTheDocument();
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(mockStreamExistingChatRun).toHaveBeenLastCalledWith("chat-123", next.activeRun!.runId, expect.anything(),
      { assistantMessageId: next.activeRun!.assistantMessageId, nextEventIndex: 12 });
    expect(mockStreamExistingChatRun.mock.lastCall?.[2].signal?.aborted).toBe(false);
  });

  it.each([401, 404, 500])("does not navigate away for a stale queue error (%s)", async status => {
    const user = userEvent.setup();
    let reject!: (error: Error) => void;
    mockQueueChatRunInput.mockImplementation(() => new Promise((_resolve, fail) => { reject = fail; }));
    mockStreamExistingChatRun.mockImplementation(() => new Promise(() => {}));
    render(<ChatView initial={makeDetail()} />);
    await user.click(screen.getByRole("button", { name: "Submit composer" }));
    const next = makeDetail({ runId: "root-b", status: "running", assistantMessageId: "assistant-b", nextEventIndex: 12 });
    next.messages[0].content = "Selected B after queue error";
    act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: next }));
    await act(async () => reject(new WebApiError(status, "queue response failed")));
    expect(screen.getByText("Selected B after queue error")).toBeInTheDocument();
    expect(pushMock).not.toHaveBeenCalled();
    expect(replaceMock).not.toHaveBeenCalled();
    expect(mockGetChat).not.toHaveBeenCalled();
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(composerSubmissionError).toBeInstanceOf(WebApiError);
    expect(mockStreamExistingChatRun.mock.lastCall?.[2].signal?.aborted).toBe(false);
  });

  it("keeps observation and reports failure when queue-conflict refresh fails", async () => {
    const user = userEvent.setup();
    mockQueueChatRunInput.mockRejectedValue(new WebApiError(409, "queue conflict"));
    mockGetChat.mockRejectedValue(new Error("refresh unavailable"));
    mockStreamExistingChatRun.mockImplementation(() => new Promise(() => {}));
    const initial = makeDetail();
    initial.messages[0].content = "Current answer";
    render(<ToastProvider><ChatView initial={initial} /></ToastProvider>);
    await user.click(screen.getByRole("button", { name: "Submit composer" }));
    await waitFor(() => expect(composerSubmissionError).toBeInstanceOf(WebApiError));
    expect(screen.getByText("Current answer")).toBeInTheDocument();
    expect(screen.getByText(/Queued input could not be confirmed/)).toBeInTheDocument();
    expect(mockGetChat).toHaveBeenCalledOnce();
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(mockStreamExistingChatRun.mock.lastCall?.[2].signal?.aborted).toBe(false);
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
      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123", { chatId: "chat-123", sessionId: "chat-123", runId: "run-123", assistantMessageId: expect.any(String) });
    });
    expect(screen.getByText("Stopped.")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Stop run" }),
    ).not.toBeInTheDocument();
    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
  });

  it("opens background task, agent, and tool activity from the chat timeline", async () => {
    const user = userEvent.setup();
    mockGetChatWorkSurface.mockResolvedValue({
      sessionId: "chat-123",
      runId: "run-123",
      taskGraph: canonicalTaskGraph(),
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

    const backgroundWork = await screen.findByLabelText("Background work");
    expect(within(backgroundWork).getAllByText("Needs attention").length).toBeGreaterThan(0);
    expect(
      screen.getByRole("button", { name: /Open tasks activity/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Open agents activity/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Open tools activity/i }),
    ).toBeInTheDocument();
    expect(screen.getAllByText("bash").length).toBeGreaterThan(0);
    expect(
      screen.getAllByText("transport disconnected").length,
    ).toBeGreaterThan(0);

    await user.click(
      screen.getByRole("button", { name: /Open tools activity/i }),
    );
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

    await user.click(
      screen.getByRole("button", { name: /Open agents activity/i }),
    );
    expect(await screen.findAllByText("No blockers")).not.toHaveLength(0);
    expect(await screen.findAllByText("Live activity")).not.toHaveLength(0);
    await waitFor(() => {
      expect(mockGetChatWorkSurfaceRun).toHaveBeenCalledWith(
        "chat-123",
        "child-run",
      );
    });
  });

  it("keeps a terminal blocked task visible after the run has left the chat header", async () => {
    const user = userEvent.setup();
    const taskGraph = canonicalTaskGraph();
    taskGraph.items.entries[0] = {
      ...taskGraph.items.entries[0]!,
      execution: {
        status: "completed",
        terminal: true,
        run: {
          run_id: "run-123",
          attempt_id: "run-123",
          graph_revision: 1,
          run_generation: 1,
          last_event_idx: 7,
          updated_at: "2026-08-04T00:00:00Z",
        },
      },
      delivery: {
        status: "blocked",
        summary: "The web capability is unavailable.",
        blocker_kind: "capability_unavailable",
        unavailable_capabilities: ["web_fetch"],
      },
    };
    mockGetChatWorkSurface.mockResolvedValue({
      sessionId: "chat-123",
      runId: "run-123",
      taskGraph,
      events: [],
      generatedAt: "2026-08-04T00:00:01Z",
    });

    render(<ChatView initial={makeDetail(null)} />);

    const backgroundWork = await screen.findByLabelText("Background work");
    expect(within(backgroundWork).getByText("Needs attention")).toBeInTheDocument();
    expect(screen.getAllByText("Blocked").length).toBeGreaterThan(0);
    await user.click(screen.getByRole("button", { name: /Open tasks activity/i }));
    expect(screen.getAllByText("Blocked").length).toBeGreaterThan(0);
    expect(screen.getByRole("button", { name: "Needs attention 1" })).toBeInTheDocument();
  });

  it("shows parent-paused subagent work as waiting in the chat timeline", async () => {
    mockGetChatWorkSurface.mockResolvedValue({
      sessionId: "chat-123",
      runId: "run-123",
      taskGraph: null,
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
          type: "run_interrupted",
          run_id: "run-123",
          kind: "empty_completion",
          resumable: true,
          timestamp: 1_801_000_001_000,
        },
      ],
      generatedAt: "2026-06-07T00:00:00.000Z",
    });

    render(<ChatView initial={makeDetail()} />);

    const card = await screen.findByLabelText("Background work");
    expect(within(card).getAllByText("Waiting").length).toBeGreaterThan(0);
    expect(within(card).getByText("Review the branch")).toBeInTheDocument();
    expect(within(card).queryByText("Needs attention")).toBeNull();
    expect(within(card).queryByText("interrupted")).toBeNull();
  });

  it("shows empty-completion subagent work as final-answer work in the chat timeline", async () => {
    mockGetChatWorkSurface.mockResolvedValue({
      sessionId: "chat-123",
      runId: "run-123",
      taskGraph: null,
      events: [
        {
          type: "agent_spawned",
          agent_id: "agent-weather",
          run_id: "child-weather",
          parent_run_id: "run-123",
          agent_type: "research",
          description: "Fetch Shanghai weather",
          timestamp: 1_801_000_000_000,
        },
        {
          type: "agent_interrupted",
          agent_id: "agent-weather",
          run_id: "child-weather",
          parent_run_id: "run-123",
          reason: "empty_completion",
          result_summary: "上海今日小雨，33°C / 25°C。",
          timestamp: 1_801_000_001_000,
        },
      ],
      generatedAt: "2026-06-07T00:00:00.000Z",
    });

    render(<ChatView initial={makeDetail()} />);

    const card = await screen.findByLabelText("Background work");
    expect(within(card).getByText("Needs final answer")).toBeInTheDocument();
    expect(within(card).getByText("Fetch Shanghai weather")).toBeInTheDocument();
    expect(within(card).queryByText("Needs attention")).toBeNull();
    expect(within(card).queryByText("Interrupted")).toBeNull();
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
        nextEventIndex: 0,
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
        nextEventIndex: 0,
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
      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123", { chatId: "chat-123", sessionId: "chat-123", runId: "run-123", assistantMessageId: expect.any(String) });
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
      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123", { chatId: "chat-123", sessionId: "chat-123", runId: "run-123", assistantMessageId: expect.any(String) });
    });
    await waitFor(() => {
      expect(mockGetChat).toHaveBeenCalledWith("chat-123");
    });
    await waitFor(() => {
      expect(screen.queryByText("Stopping")).not.toBeInTheDocument();
    });
    expect(screen.getAllByText("Working").length).toBeGreaterThan(0);
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

      expect(mockStopChatRun).toHaveBeenCalledWith("chat-123", { chatId: "chat-123", sessionId: "chat-123", runId: "run-123", assistantMessageId: expect.any(String) });
      await act(async () => {
        vi.advanceTimersByTime(10_000);
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(mockGetChat).toHaveBeenCalledWith("chat-123");
      expect(screen.queryByText("Stopping")).not.toBeInTheDocument();
    expect(screen.getAllByText("Working").length).toBeGreaterThan(0);
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

      const initial = makeDetail();
      mockGetChat.mockResolvedValue(initial);
      render(<ChatView initial={initial} />);

      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(1);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_000);
      });

      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("bounds interrupted GETs and resets the budget on authoritative pause/resume", async () => {
    vi.useFakeTimers();
    const actual = await vi.importActual<typeof import("@/lib/api/chats")>("@/lib/api/chats");
    mockStreamExistingChatRun.mockImplementation(actual.streamExistingChatRun);
    const initial = makeDetail();
    initial.messages[0].content = "Saved partial answer";
    mockGetChat.mockResolvedValue(initial);
    const fetch = vi.spyOn(globalThis, "fetch").mockImplementation(async () => new Response(
      'data: {"type":"session_info","session_id":"chat-123","run_id":"run-123"}\n\n'));
    try {
      render(<ToastProvider><ChatView initial={initial} /></ToastProvider>);
      for (let elapsed = 0; elapsed <= 20_000 && mockStreamExistingChatRun.mock.calls.length < 5; elapsed += 1_000) {
        await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
      }
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(5);
      expect(screen.getByRole("alert")).toHaveTextContent("The run stream was interrupted");
      await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(5);
      mockStopChatRun.mockResolvedValue({ cancelPending: true,
        activeRun: { ...initial.activeRun!, status: "cancelling" } });
      await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Stop run" })); });
      await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(5);
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
      const paused = { ...initial, activeRun: { ...initial.activeRun!, status: "paused" } };
      mockGetChat.mockResolvedValue(paused);
      act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: paused }));
      mockGetChat.mockResolvedValue(initial);
      act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: initial }));
      for (let elapsed = 0; elapsed <= 20_000 && mockStreamExistingChatRun.mock.calls.length < 10; elapsed += 1_000) {
        await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
      }
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(10);
      expect(screen.getByRole("alert")).toHaveTextContent("The run stream was interrupted");
      await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(10);
      expect(screen.getByText("Saved partial answer")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
      expect(mockStreamChatMessage).not.toHaveBeenCalled();
      expect(mockStopChatRun).toHaveBeenCalledOnce();
    } finally { fetch.mockRestore(); vi.useRealTimers(); }
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
            assistantMessageId: "assistant-blocked",
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
        nextEventIndex: 0,
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
        nextEventIndex: 0,
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
        assistantMessageId: "assistant-1",
        nextEventIndex: 8,
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

    expect(screen.getByText("Run paused")).toBeInTheDocument();
    expect(
      screen.getByText(
        "Continue this run or close it before changing direction.",
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
        assistantMessageId: "assistant-paused",
        nextEventIndex: 8,
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

  it("continues the authoritative completed paused assistant through the real browser consumer", async () => {
    const user = userEvent.setup();
    mockResumeChatRun.mockResolvedValue({ activeRun: {
      runId: "run-123", status: "running", assistantMessageId: "assistant-paused", nextEventIndex: 8,
    } });
    const actual = await vi.importActual<typeof import("@/lib/api/chats")>("@/lib/api/chats");
    mockStreamExistingChatRun.mockImplementation(actual.streamExistingChatRun);
    const events = [
      { type: "chat_stream_init", assistant_message_id: "assistant-paused", session_id: "chat-123", state: {
        runId: "run-123", assistantRawText: "Paused partial", assistantText: "Paused partial", reasoningText: "Saved reasoning",
        lastStatus: "complete", runLifecycle: "paused", terminalStatus: "paused", nextEventIndex: 8,
      } },
      { type: "session_info", session_id: "chat-123", run_id: "run-123" },
      { type: "run_resumed", run_id: "run-123", index: 8 },
      { type: "text_delta", content: " continued", index: 9 },
      { type: "run_finished", run_id: "run-123", status: "completed", index: 10 },
    ];
    const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(
      events.map(event => `data: ${JSON.stringify(event)}\n\n`).join(""),
    ));
    try {
      const { container } = render(<ChatView initial={{
        ...makeDetail({ runId: "run-123", status: "paused", assistantMessageId: "assistant-paused" }),
        messages: [
          { id: "assistant-previous", role: "assistant", content: "Previous complete answer", createdAt: "2026-06-07T00:00:00.000Z", status: "complete" },
          { id: "assistant-paused", role: "assistant", content: "Paused partial", reasoning: "Saved reasoning",
            createdAt: "2026-06-07T00:00:01.000Z", status: "complete", reasoningStatus: "complete" },
        ],
      }} />);
      await user.click(screen.getByRole("button", { name: "Resume" }));
      await waitFor(() => expect(screen.getByText("Paused partial continued")).toBeInTheDocument());
      expect(mockStreamExistingChatRun).toHaveBeenCalledExactlyOnceWith("chat-123", "run-123", expect.anything(), {
        assistantMessageId: "assistant-paused", nextEventIndex: 8,
      });
      expect(screen.getByText("Paused partial continued")).toHaveAttribute("data-reasoning", "Saved reasoning");
      expect(screen.getByText("Previous complete answer")).toBeInTheDocument();
      expect(container.querySelectorAll("[data-reasoning]")).toHaveLength(2);
      expect(mockGetChat).not.toHaveBeenCalled();
    } finally { fetch.mockRestore(); }
  });

  it.each([
    { point: "reply", committed: false }, { point: "refresh", committed: false },
    { point: "reply", committed: true }, { point: "refresh", committed: true },
  ])("observes the committed newer binding after delayed $point (committed=$committed)", async ({ point, committed }) => {
    const user = userEvent.setup();
    const initial: ChatDetail = {
      ...makeDetail({ runId: "run-123", status: "paused", assistantMessageId: "assistant-a" }),
      messages: [
        { id: "assistant-a", role: "assistant", content: "Paused answer A", createdAt: "2026-06-07T00:00:00.000Z", status: "complete" },
        { id: "assistant-b", role: "assistant", content: "New selected answer B", createdAt: "2026-06-07T00:00:01.000Z", status: "complete" },
      ],
    };
    const response = { activeRun: { runId: "run-123", status: "running", assistantMessageId: "assistant-a", nextEventIndex: 8 } };
    let reply!: (value: Awaited<ReturnType<typeof resumeChatRun>>) => void;
    let refresh!: (value: ChatDetail) => void;
    mockResumeChatRun.mockImplementation(() => point === "reply" ? new Promise(resolve => { reply = resolve; })
      : Promise.resolve({ activeRun: { runId: "run-123", status: "running" } }));
    mockGetChat.mockImplementation(() => new Promise(resolve => { refresh = resolve; }));
    mockStreamExistingChatRun.mockImplementation(() => new Promise(() => {}));
    render(<ChatView initial={initial} />);
    await user.click(screen.getByRole("button", { name: "Resume" }));
    if (point === "refresh") await waitFor(() => expect(mockGetChat).toHaveBeenCalled());
    const replaceBinding = () => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: {
      ...initial, activeRun: { runId: "run-123", status: "running", assistantMessageId: "assistant-b", nextEventIndex: 12 },
    } });
    if (committed) act(replaceBinding);
    await act(async () => {
      if (!committed) replaceBinding();
      if (point === "reply") reply(response);
      else refresh({ ...initial, activeRun: response.activeRun });
    });
    await waitFor(() => expect(mockStreamExistingChatRun).toHaveBeenCalledExactlyOnceWith("chat-123", "run-123", expect.anything(),
      { assistantMessageId: "assistant-b", nextEventIndex: 12 }));
    expect(mockStreamExistingChatRun.mock.calls[0][2].signal?.aborted).toBe(false);
    expect(screen.getByText("New selected answer B")).toBeInTheDocument();
    expect(screen.getByText("Paused answer A")).toBeInTheDocument();
  });

  it.each([false, true])("scopes pending POST transcript recovery to its observer (replaced=%s)", async replaced => {
    vi.useFakeTimers();
    try {
      const userMessage = { id: "pending-user", role: "user" as const, content: "First turn", createdAt: "2026-06-07T00:00:00Z", status: "complete" as const };
      const assistant = { id: "assistant-a", role: "assistant" as const, content: "", createdAt: "2026-06-07T00:00:01Z", status: "streaming" as const };
      let resolveTranscript!: (detail: ChatDetail) => void;
      mockGetChat.mockImplementation(() => new Promise(resolve => { resolveTranscript = resolve; }));
      mockStreamChatMessage.mockImplementation((_chat, _payload, handlers) => {
        handlers.onLocalMessages?.({ userMessage, assistantMessage: assistant });
        handlers.onRunStarted?.("root-a");
        return new Promise(() => {});
      });
      mockStreamExistingChatRun.mockImplementation(() => new Promise(() => {}));
      const initial: ChatDetail = { ...makeDetail(null), messages: [userMessage], pendingTurn: {
        messageId: userMessage.id, content: userMessage.content, options: composerPayload.options,
      } };
      render(<ChatView initial={initial} />);
      await act(async () => { await vi.advanceTimersByTimeAsync(0); });
      expect(mockStreamChatMessage).toHaveBeenCalledOnce();
      const creation = mockStreamChatMessage.mock.calls[0][2].signal!;
      await act(async () => { await vi.advanceTimersByTimeAsync(3_000); });
      expect(mockGetChat).toHaveBeenCalledOnce();
      let replacement: AbortSignal | undefined;
      if (replaced) {
        const next = makeDetail({ runId: "root-b", status: "running", assistantMessageId: "assistant-b", nextEventIndex: 12 });
        next.messages[0].content = "Selected B";
        act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: next }));
        await act(async () => { await vi.advanceTimersByTimeAsync(0); });
        expect(mockStreamExistingChatRun).toHaveBeenCalledOnce();
        replacement = mockStreamExistingChatRun.mock.calls[0][2].signal;
      }
      await act(async () => resolveTranscript({ ...makeDetail(null), messages: [userMessage,
        { ...assistant, content: "Recovered A", status: "complete" }],
      }));
      if (replaced) {
        expect(replacement?.aborted).toBe(false);
        expect(screen.getByText("Selected B")).toBeInTheDocument();
        expect(screen.queryByText("Recovered A")).not.toBeInTheDocument();
        expect(mockStreamExistingChatRun).toHaveBeenCalledOnce();
      } else {
        expect(creation.aborted).toBe(true);
        expect(screen.getByText("Recovered A")).toBeInTheDocument();
        expect(mockStreamExistingChatRun).not.toHaveBeenCalled();
        await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
        expect(mockGetChat).toHaveBeenCalledOnce();
      }
      expect(mockStopChatRun).not.toHaveBeenCalled();
    } finally { vi.useRealTimers(); }
  });

  it("admits one observer in StrictMode and none after immediate unmount", async () => {
    mockStreamExistingChatRun.mockImplementation(() => new Promise(() => {}));
    const first = render(<StrictMode><ChatView initial={makeDetail()} /></StrictMode>);
    first.unmount();
    await act(async () => { await Promise.resolve(); });
    expect(mockStreamExistingChatRun).not.toHaveBeenCalled();
    const second = render(<StrictMode><ChatView initial={makeDetail()} /></StrictMode>);
    await waitFor(() => expect(mockStreamExistingChatRun).toHaveBeenCalledOnce());
    const signal = mockStreamExistingChatRun.mock.calls[0][2].signal!;
    second.unmount();
    expect(signal.aborted).toBe(true);
    expect(mockStopChatRun).not.toHaveBeenCalled();
  });

  it.each(["message", "root", "session"])("fences stale callbacks, RAF and cleanup after %s replacement", async change => {
    const frames = mockAnimationFrameQueue();
    const first = makeDetail({ ...defaultActiveRun, assistantMessageId: "assistant-a" });
    first.messages = [
      { id: "assistant-a", role: "assistant", content: "Saved A", createdAt: "2026-06-07T00:00:00Z", status: "streaming" },
      { id: "assistant-b", role: "assistant", content: "Saved B", createdAt: "2026-06-07T00:00:01Z", status: "streaming" },
    ];
    const next: ChatDetail = { ...first, session: { ...first.session!, backendSessionId: change === "session" ? "next-session" : "chat-123" },
      activeRun: { runId: change === "root" ? "next-root" : "run-123", status: "running", assistantMessageId: "assistant-b", nextEventIndex: 12 } };
    let finish!: () => void;
    let fail!: (error: Error) => void;
    mockStreamExistingChatRun.mockImplementationOnce(() => new Promise((resolve, reject) => {
      finish = () => resolve(""); fail = reject;
    })).mockImplementation(() => new Promise(() => {}));
    try {
      render(<ChatView initial={first} />);
      await waitFor(() => expect(mockStreamExistingChatRun).toHaveBeenCalledOnce());
      const old = mockStreamExistingChatRun.mock.calls[0][2];
      act(() => old.onText?.("Pending stale A"));
      act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: next }));
      await waitFor(() => expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(2));
      const current = mockStreamExistingChatRun.mock.calls[1][2];
      expect(old.signal?.aborted).toBe(true);
      act(() => {
        old.onRunUpdated?.({ runId: "run-123", status: "running" });
        old.onText?.("Late stale A"); old.onDone?.("Stale terminal");
        frames.flushAll();
      });
      await act(async () => {
        if (change === "message") finish();
        else fail(change === "root" ? new WebApiError(500, "Stale failure", "canonical_persistence_failed") : new Error("stale socket"));
      });
      act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: {
        ...next, activeRun: { ...next.activeRun!, status: "waiting", nextEventIndex: 20 },
      } }));
      expect(current.signal?.aborted).toBe(false);
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(2);
      expect(screen.getByText("Saved A")).toBeInTheDocument();
      expect(screen.getByText("Saved B")).toBeInTheDocument();
      expect(screen.queryByText(/Stale terminal|Late stale|Pending stale/)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
      expect(mockStopChatRun).not.toHaveBeenCalled();
    } finally { frames.restore(); }
  });

  it("retires the old binding's retry timer when a new message is selected", async () => {
    vi.useFakeTimers();
    try {
      const first = makeDetail();
      mockGetChat.mockResolvedValue(first);
      mockStreamExistingChatRun.mockRejectedValueOnce(new Error("temporary stream failure"))
        .mockImplementation(() => new Promise(() => {}));
      render(<ChatView initial={first} />);
      await act(async () => { await vi.advanceTimersByTimeAsync(0); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledOnce();
      const next = { ...first, activeRun: { ...first.activeRun!, assistantMessageId: "assistant-next", nextEventIndex: 12 },
        messages: [{ id: "assistant-next", role: "assistant" as const, content: "New answer", createdAt: "2026-06-07T00:00:01Z", status: "streaming" as const }] };
      act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: next }));
      await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(2);
      expect(mockStreamExistingChatRun.mock.calls[1][3]).toMatchObject({ assistantMessageId: "assistant-next", nextEventIndex: 12 });
    } finally { vi.useRealTimers(); }
  });

  it("registers the live POST with the same owner and retains trailing authoritative text", async () => {
    const user = userEvent.setup();
    const actual = await vi.importActual<typeof import("@/lib/api/chats")>("@/lib/api/chats");
    mockStreamChatMessage.mockImplementation(actual.streamChatMessage);
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({ start(value) { controller = value; } });
    const send = (events: object[]) => controller.enqueue(new TextEncoder().encode(events.map(event => `data: ${JSON.stringify(event)}\n\n`).join("")));
    const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(stream));
    try {
      render(<ChatView initial={makeDetail(null)} />);
      await user.click(screen.getByRole("button", { name: "Submit composer" }));
      await act(async () => send([
        { type: "session_info", session_id: "created-session", run_id: "created-root" },
        { type: "text_delta", content: "Partial POST answer", index: 8 },
      ]));
      await waitFor(() => expect(screen.getByText("Partial POST answer")).toBeInTheDocument());
      expect(mockStreamExistingChatRun).not.toHaveBeenCalled();
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
      await act(async () => send([{ type: "run_finished", run_id: "created-root", status: "completed", index: 9 }]));
      await act(async () => {
        send([{ type: "turn_complete", assistant_text: "Authoritative POST answer", index: 10 }]); controller.close();
      });
      await waitFor(() => expect(screen.getByText("Authoritative POST answer")).toBeInTheDocument());
      expect(mockStreamChatMessage).toHaveBeenCalledOnce();
      expect(mockStreamExistingChatRun).not.toHaveBeenCalled();
    } finally { fetch.mockRestore(); }
  });

  it.each([
    { mode: "POST", refresh: true }, { mode: "POST", refresh: false },
    { mode: "GET", refresh: true }, { mode: "GET", refresh: false },
  ])("recovers an interrupted $mode through GET while preserving output (refresh=$refresh)", async ({ mode, refresh }) => {
    const actual = await vi.importActual<typeof import("@/lib/api/chats")>("@/lib/api/chats");
    mockStreamChatMessage.mockImplementation(actual.streamChatMessage);
    mockStreamExistingChatRun.mockImplementation(actual.streamExistingChatRun);
    const user = userEvent.setup();
    const assistant = { id: "assistant-observed", role: "assistant" as const, content: "Partial answer",
      reasoning: "Saved reasoning", status: "streaming" as const, createdAt: "2026-06-07T00:00:00Z" };
    const observed = { ...makeDetail({ ...defaultActiveRun, assistantMessageId: assistant.id, nextEventIndex: 8 }),
      messages: [assistant] };
    if (refresh) mockGetChat.mockResolvedValue(observed);
    else mockGetChat.mockRejectedValue(new Error("refresh unavailable"));
    const encode = (events: object[]) => new TextEncoder().encode(
      events.map(event => `data: ${JSON.stringify(event)}\n\n`).join(""));
    const interrupted = encode([
      ...(mode === "POST" ? [{ type: "local_messages", assistant_message: { ...assistant, content: "", reasoning: "" },
        user_message: { id: "user-observed", role: "user", content: "Start", createdAt: assistant.createdAt } }] : []),
      { type: "session_info", session_id: "chat-123", run_id: "run-123" },
      { type: "text_delta", content: assistant.content },
      { type: "reasoning_delta", content: assistant.reasoning },
      { type: "run_input_queued", run_id: "run-123", index: 7 },
    ]);
    let continuation!: ReadableStreamDefaultController<Uint8Array>;
    const body = new ReadableStream<Uint8Array>({ start(controller) { continuation = controller; } });
    const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValueOnce(new Response(interrupted))
      .mockResolvedValue(new Response(body));
    try {
      render(<ToastProvider><ChatView initial={mode === "POST" ? makeDetail(null) : observed} /></ToastProvider>);
      if (mode === "POST") await user.click(screen.getByRole("button", { name: "Submit composer" }));
      await waitFor(() => expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(mode === "POST" ? 1 : 2), { timeout: 3_000 });
      expect(screen.getByText(assistant.content)).toHaveAttribute("data-reasoning", assistant.reasoning);
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
      expect(screen.queryByText(/could not reach the Astra runtime/)).not.toBeInTheDocument();
      expect(mockStreamExistingChatRun).toHaveBeenLastCalledWith("chat-123", "run-123", expect.anything(),
        { assistantMessageId: assistant.id, nextEventIndex: 8 });
      await act(async () => {
        continuation.enqueue(encode([{ type: "text_done", full_text: "Completed answer" },
          { type: "run_finished", run_id: "run-123", status: "completed", index: 8 }]));
        continuation.close();
      });
      await waitFor(() => expect(screen.getByText("Completed answer")).toBeInTheDocument());
      expect(mockStreamChatMessage).toHaveBeenCalledTimes(mode === "POST" ? 1 : 0);
      expect(mockStopChatRun).not.toHaveBeenCalled();
    } finally { fetch.mockRestore(); }
  });

  it.each(["chat", "upstream run"])("keeps missing-resource navigation scoped to the %s", async owner => {
    const actual = await vi.importActual<typeof import("@/lib/api/chats")>("@/lib/api/chats");
    mockStreamExistingChatRun.mockImplementationOnce(actual.streamExistingChatRun)
      .mockImplementation(() => new Promise(() => {}));
    const initial = makeDetail();
    initial.messages[0].content = "Saved partial answer";
    mockGetChat.mockResolvedValue(initial);
    const response = owner === "chat" ? new Response("Chat not found", { status: 404 }) : new Response(
      'data: {"type":"error","message":"The run stream was interrupted.","status":502,"code":"stream_observation_interrupted"}\n\n');
    const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(response);
    try {
      render(<ToastProvider><ChatView initial={initial} /></ToastProvider>);
      if (owner === "chat") {
        await waitFor(() => expect(replaceMock).toHaveBeenCalledOnce());
        expect(mockGetChat).not.toHaveBeenCalled();
      } else {
        await waitFor(() => expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(2), { timeout: 3_000 });
        expect(replaceMock).not.toHaveBeenCalled();
        expect(screen.getByText("Saved partial answer")).toBeInTheDocument();
        expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
      }
    } finally { fetch.mockRestore(); }
  });

  it.each([true, false])("refreshes a missing resume binding once (recovered=%s)", async recovered => {
    const user = userEvent.setup();
    const initial: ChatDetail = {
      ...makeDetail({ runId: "run-123", status: "paused", assistantMessageId: "assistant-paused" }),
      messages: [{ id: "assistant-paused", role: "assistant", content: "Saved partial", reasoning: "Saved reasoning",
        createdAt: "2026-06-07T00:00:00.000Z", status: "complete" }],
    };
    mockResumeChatRun.mockResolvedValue({ activeRun: { runId: "run-123", status: "running" } });
    mockGetChat.mockResolvedValue({ ...initial, activeRun: {
      runId: "run-123", status: "running",
      ...(recovered ? { assistantMessageId: "assistant-paused", nextEventIndex: 8 } : {}),
    } });
    mockStreamExistingChatRun.mockImplementation(async (_chat, _run, handlers) => {
      handlers.onText?.("Recovered answer"); handlers.onDone?.("Recovered answer"); return "Recovered answer";
    });
    const { container } = render(<ToastProvider><ChatView initial={initial} /></ToastProvider>);
    await user.click(screen.getByRole("button", { name: "Resume" }));
    if (recovered) {
      await waitFor(() => expect(screen.getByText("Recovered answer")).toBeInTheDocument());
      expect(mockStreamExistingChatRun).toHaveBeenCalledExactlyOnceWith("chat-123", "run-123", expect.anything(), {
        assistantMessageId: "assistant-paused", nextEventIndex: 8,
      });
    } else {
      await waitFor(() => expect(screen.getByText(/The run resumed, but its stream continuation is unavailable/)).toBeInTheDocument());
      expect(mockStreamExistingChatRun).not.toHaveBeenCalled();
      expect(screen.getByText("Saved partial")).toHaveAttribute("data-reasoning", "Saved reasoning");
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
    }
    expect(mockGetChat).toHaveBeenCalledExactlyOnceWith("chat-123");
    expect(container.querySelectorAll("[data-reasoning]")).toHaveLength(1);
    expect(mockStopChatRun).not.toHaveBeenCalled();
  });

  it("refreshes chat detail when a resumed run cannot reconnect to the stream", async () => {
    const user = userEvent.setup();
    mockResumeChatRun.mockResolvedValue({
      activeRun: {
        runId: "run-123",
        status: "running",
        waitingFor: null,
        assistantMessageId: "assistant-paused",
        nextEventIndex: 8,
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
    expect(screen.getByText("Run paused")).toBeInTheDocument();
    expect(mockGetChat).toHaveBeenCalledExactlyOnceWith("chat-123");
    expect(mockStreamExistingChatRun).toHaveBeenCalledOnce();
  });

  it.each([
    { point: "reply", replaced: false }, { point: "reply", replaced: "root" },
    { point: "refresh", replaced: false }, { point: "refresh", replaced: "root" },
    { point: "reply", replaced: "session" }, { point: "refresh", replaced: "message" },
  ])("fences Stop $point after binding replacement (replaced=$replaced)", async ({ point, replaced }) => {
    const initial = makeDetail();
    initial.messages[0].content = "Original answer A";
    const next = makeDetail({ runId: replaced === "root" ? "new-root" : "run-123", status: "running",
      assistantMessageId: replaced === "session" ? "assistant-live" : "assistant-b", nextEventIndex: 12 });
    if (replaced === "session") next.session!.backendSessionId = "new-session";
    next.messages[0].content = "Selected answer B";
    let reply!: () => void;
    let refresh!: (value: ChatDetail) => void;
    mockStopChatRun.mockImplementation(() => new Promise((resolve, reject) => {
      reply = () => point === "reply" ? resolve({}) : reject(new Error("Stop rejected"));
    }));
    mockGetChat.mockImplementation(() => new Promise(resolve => { refresh = resolve; }));
    mockStreamExistingChatRun.mockImplementation(() => new Promise(() => {}));
    render(<ToastProvider><ChatView initial={initial} /></ToastProvider>);
    await waitFor(() => expect(mockStreamExistingChatRun).toHaveBeenCalledOnce());
    await userEvent.setup().click(screen.getByRole("button", { name: "Stop run" }));
    expect(mockStopChatRun).toHaveBeenCalledExactlyOnceWith("chat-123", { chatId: "chat-123", sessionId: "chat-123", runId: "run-123", assistantMessageId: expect.any(String) });
    if (point === "refresh") {
      await act(async () => reply());
      await waitFor(() => expect(mockGetChat).toHaveBeenCalledOnce());
    }
    if (replaced) act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: next }));
    await act(async () => { if (point === "reply") reply(); else refresh(initial); });
    if (replaced) {
      expect(screen.getByText("Selected answer B")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
      expect(mockStreamExistingChatRun).toHaveBeenLastCalledWith("chat-123", next.activeRun!.runId, expect.anything(),
        { assistantMessageId: next.activeRun!.assistantMessageId, nextEventIndex: 12 });
    } else if (point === "reply") {
      expect(screen.queryByRole("button", { name: "Stop run" })).not.toBeInTheDocument();
    } else {
      expect(screen.getByText("Original answer A")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
    }
  });

  it("releases a replaced Stop promptly without letting its finalizer unlock the successor", async () => {
    let settleA!: () => void;
    let settleB!: () => void;
    mockStopChatRun.mockImplementationOnce(() => new Promise(resolve => { settleA = () => resolve({}); }))
      .mockImplementationOnce(() => new Promise(resolve => { settleB = () => resolve({}); }));
    mockStreamExistingChatRun.mockImplementation(() => new Promise(() => {}));
    render(<ChatView initial={makeDetail()} />);
    await userEvent.setup().click(screen.getByRole("button", { name: "Stop run" }));
    const next = makeDetail({ ...defaultActiveRun, runId: "new-root", assistantMessageId: "assistant-b" });
    act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: next }));
    expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
    await userEvent.setup().click(screen.getByRole("button", { name: "Stop run" }));
    await act(async () => settleA());
    expect(screen.getByRole("button", { name: "Submit composer" })).toBeDisabled();
    expect(mockStopChatRun).toHaveBeenCalledTimes(2);
    await act(async () => settleB());
    expect(screen.getByRole("button", { name: "Submit composer" })).toBeEnabled();
  });

  it("retires Stop effects on unmount before a late pending ACK", async () => {
    vi.useFakeTimers();
    try {
      let settle!: () => void;
      mockStopChatRun.mockImplementation(() => new Promise(resolve => {
        settle = () => resolve({ cancelPending: true, activeRun: { ...defaultActiveRun, status: "cancelling" } });
      }));
      const { unmount } = render(<ChatView initial={makeDetail()} />);
      await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Stop run" })); });
      unmount();
      await act(async () => settle());
      await act(async () => { await vi.advanceTimersByTimeAsync(20_000); });
      expect(mockGetChat).not.toHaveBeenCalled();
      expect(pushMock).not.toHaveBeenCalled();
      expect(replaceMock).not.toHaveBeenCalled();
    } finally { vi.useRealTimers(); }
  });

  it("keeps Stop reconciliation single-flight and discards a superseded read", async () => {
    vi.useFakeTimers();
    try {
      let settle!: (value: ChatDetail) => void;
      mockGetChat.mockImplementation(() => new Promise(resolve => { settle = resolve; }));
      mockStopChatRun.mockResolvedValue({ cancelPending: true, activeRun: { ...defaultActiveRun, status: "cancelling" } });
      render(<ChatView initial={makeDetail()} />);
      await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Stop run" })); });
      await act(async () => { await vi.advanceTimersByTimeAsync(500); });
      expect(mockGetChat).toHaveBeenCalledOnce();
      await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
      expect(mockGetChat).toHaveBeenCalledOnce();
      const next = makeDetail({ ...defaultActiveRun, runId: "new-root", assistantMessageId: "assistant-b" });
      next.messages[0].content = "Successor output";
      act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: next }));
      await act(async () => settle(makeDetail()));
      await act(async () => { await vi.advanceTimersByTimeAsync(20_000); });
      expect(mockGetChat).toHaveBeenCalledOnce();
      expect(screen.getByText("Successor output")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Stop run" })).toBeEnabled();
    } finally { vi.useRealTimers(); }
  });


  it.each(["ACK", "failure refresh"])("preserves a newer terminal observation across Stop %s", async point => {
    let settle!: () => void;
    let refresh!: (value: ChatDetail) => void;
    mockStopChatRun.mockImplementation(() => new Promise((resolve, reject) => {
      settle = () => point === "ACK" ? resolve({}) : reject(new Error("Stop rejected"));
    }));
    mockGetChat.mockImplementation(() => new Promise(resolve => { refresh = resolve; }));
    render(<ToastProvider><ChatView initial={makeDetail()} /></ToastProvider>);
    await userEvent.setup().click(screen.getByRole("button", { name: "Stop run" }));
    if (point === "failure refresh") await act(async () => settle());
    const terminal = makeDetail(null);
    terminal.messages = [{ id: "assistant-live", role: "assistant", content: "New canonical result",
      createdAt: "2026-06-07T00:00:00Z", status: "complete" }];
    act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: terminal }));
    await act(async () => { if (point === "ACK") settle(); else refresh(makeDetail()); });
    expect(screen.getByText("New canonical result")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Stop run" })).not.toBeInTheDocument();
    expect(screen.queryByText("Stop rejected")).not.toBeInTheDocument();
  });


  it.each([
    { pending: false, outcome: "running", refreshed: true, reconnect: true },
    { pending: false, outcome: "running", refreshed: false, reconnect: true },
    ...["running", "waiting", "blocked", "input-queued"].map(outcome =>
      ({ pending: true, outcome, refreshed: true, reconnect: true })),
    ...["paused", "terminal"].map(outcome =>
      ({ pending: true, outcome, refreshed: true, reconnect: false })),
  ])("recovers Stop observation (pending=$pending, outcome=$outcome, refresh=$refreshed)", async ({ pending, outcome, refreshed, reconnect }) => {
    vi.useFakeTimers();
    try {
      const initial = makeDetail({ ...defaultActiveRun, nextEventIndex: 8 });
      initial.messages[0].content = "Saved partial output";
      mockStreamExistingChatRun.mockImplementationOnce(() => new Promise(() => {}))
        .mockImplementationOnce((_chat, _run, handlers) => {
          handlers.onText?.("Recovered live output");
          return new Promise(() => {});
        });
      if (pending) mockStopChatRun.mockResolvedValue({ cancelPending: true,
        activeRun: { ...defaultActiveRun, status: "cancelling", nextEventIndex: 8 } });
      else mockStopChatRun.mockRejectedValue(new Error("Stop rejected"));
      if (refreshed) mockGetChat.mockResolvedValue({ ...initial,
        activeRun: outcome === "terminal" ? undefined : { ...initial.activeRun!, status: outcome } });
      else mockGetChat.mockRejectedValue(new Error("Refresh unavailable"));
      render(<ToastProvider><ChatView initial={initial} /></ToastProvider>);
      await act(async () => { await Promise.resolve(); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledOnce();
      await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Stop run" })); });
      expect(mockStreamExistingChatRun.mock.calls[0][2].signal?.aborted).toBe(true);
      if (pending) await act(async () => { await vi.advanceTimersByTimeAsync(500); });
      expect(mockGetChat).toHaveBeenCalledOnce();
      await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(reconnect ? 2 : 1);
      if (reconnect) {
        expect(mockStreamExistingChatRun).toHaveBeenLastCalledWith("chat-123", "run-123", expect.anything(),
          { assistantMessageId: "assistant-live", nextEventIndex: 8 });
        await act(async () => { await vi.advanceTimersByTimeAsync(50); });
        expect(screen.getByText("Recovered live output")).toBeInTheDocument();
      } else if (outcome === "paused") {
        expect(screen.getByRole("button", { name: "Resume" })).toBeEnabled();
      } else {
        expect(screen.queryByRole("button", { name: "Stop run" })).not.toBeInTheDocument();
      }
      expect(mockStreamChatMessage).not.toHaveBeenCalled();
    } finally { vi.useRealTimers(); }
  });


  it("recovers only the committed fresh Stop selection and cursor", async () => {
    vi.useFakeTimers();
    try {
      const initial = makeDetail({ ...defaultActiveRun, nextEventIndex: 8 });
      initial.messages[0].content = "Original output";
      const polls: Array<(value: ChatDetail) => void> = [];
      mockGetChat.mockImplementation(() => new Promise(resolve => polls.push(resolve)));
      mockStopChatRun.mockResolvedValue({ cancelPending: true,
        activeRun: { ...initial.activeRun!, status: "cancelling" } });
      mockStreamExistingChatRun.mockImplementationOnce(() => new Promise(() => {}))
        .mockImplementationOnce((_chat, _run, handlers) => {
          handlers.onText?.("Fresh live output");
          return new Promise(() => {});
        });
      render(<ChatView initial={initial} />);
      await act(async () => { await Promise.resolve(); });
      await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Stop run" })); });
      await act(async () => { await vi.advanceTimersByTimeAsync(500); });
      const current = makeDetail({ ...defaultActiveRun, status: "cancelling", nextEventIndex: 12 });
      current.messages[0].content = "Newer partial output";
      act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: current }));
      await act(async () => polls[0](initial));
      expect(screen.getByText("Newer partial output")).toBeInTheDocument();
      await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
      expect(mockGetChat).toHaveBeenCalledTimes(2);
      expect(mockStreamExistingChatRun).toHaveBeenCalledOnce();
      await act(async () => polls[1]({ ...current, activeRun: { ...current.activeRun!, status: "running" } }));
      await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
      expect(mockStreamExistingChatRun).toHaveBeenLastCalledWith("chat-123", "run-123", expect.anything(),
        { assistantMessageId: "assistant-live", nextEventIndex: 12 });
      await act(async () => { await vi.advanceTimersByTimeAsync(50); });
      expect(screen.getByText("Fresh live output")).toBeInTheDocument();
      expect(mockStreamChatMessage).not.toHaveBeenCalled();
    } finally { vi.useRealTimers(); }
  });

  it("hands Stop recovery back to ordinary observation exactly once", async () => {
    vi.useFakeTimers();
    try {
      const blocked = makeDetail({ ...defaultActiveRun, status: "blocked", nextEventIndex: 8 });
      mockGetChat.mockResolvedValue(blocked);
      mockStopChatRun.mockResolvedValue({ cancelPending: true,
        activeRun: { ...defaultActiveRun, status: "cancelling", nextEventIndex: 8 } });
      mockStreamExistingChatRun.mockImplementationOnce(() => new Promise(() => {}))
        .mockImplementationOnce(async (_chat, _run, handlers) => {
          handlers.onPaused?.("Blocked output");
          return "Blocked output";
        });
      render(<ChatView initial={makeDetail()} />);
      await act(async () => { await Promise.resolve(); });
      await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Stop run" })); });
      await act(async () => { await vi.advanceTimersByTimeAsync(500); });
      await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(2);
      blocked.messages[0].status = "complete";
      blocked.messages[0].content = "Blocked output";
      act(() => emitChatLifecycleChange({ action: "unarchive", chatId: "chat-123", chat: { ...blocked } }));
      await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
      expect(mockStreamExistingChatRun).toHaveBeenCalledTimes(2);
      expect(mockGetChat).toHaveBeenCalledOnce();
      expect(screen.getByText("Blocked output")).toBeInTheDocument();
      expect(mockStreamChatMessage).not.toHaveBeenCalled();
    } finally { vi.useRealTimers(); }
  });

});

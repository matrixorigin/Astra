/**
 * @jest-environment node
 */

jest.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: jest.fn(),
}));

jest.mock("@/lib/api/web-store", () => ({
  StaleDeferredRunError: class StaleDeferredRunError extends Error {},
  getChatHydrated: jest.fn(),
  queueDeferredRunInput: jest.fn(),
}));

jest.mock("@/lib/runtime-client", () => ({
  RuntimeClientError: class RuntimeClientError extends Error {
    status?: number;
    detail: string;

    constructor({ status, detail }: { status?: number; detail: string }) {
      super(detail);
      this.status = status;
      this.detail = detail;
    }
  },
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  getChatHydrated,
  queueDeferredRunInput,
} from "@/lib/api/web-store";

const mockRequireRuntimeUser = requireRuntimeUser as jest.MockedFunction<
  typeof requireRuntimeUser
>;
const mockGetChatHydrated = getChatHydrated as jest.MockedFunction<
  typeof getChatHydrated
>;
const mockQueueDeferredRunInput =
  queueDeferredRunInput as jest.MockedFunction<typeof queueDeferredRunInput>;

function activeChat(workspaceSelection?: unknown) {
  return {
    chat: {
      id: "chat-1",
      title: "Chat",
      projectId: null,
      createdAt: "2026-06-07T00:00:00.000Z",
      updatedAt: "2026-06-07T00:00:00.000Z",
      archivedAt: null,
      model: "sonnet-4.6-adaptive",
    },
    session: {
      chatId: "chat-1",
      backendSessionId: "session-1",
      persisted: true,
      messageCount: 2,
    },
    messages: [],
    activeRun: {
      runId: "run-1",
      status: "running",
      waitingFor: null,
    },
    workspaceSelection,
    workspaceSelectionExplicit: Boolean(workspaceSelection),
  };
}

function postInput(body: Record<string, unknown>) {
  return new Request("http://web.test/api/chats/chat-1/input", {
    method: "POST",
    body: JSON.stringify(body),
  }) as never;
}

describe("chat deferred input route workspace authority", () => {
  beforeEach(() => {
    jest.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
      response: null,
    } as never);
    mockQueueDeferredRunInput.mockResolvedValue({
      userMessage: {
        id: "user-queued",
        role: "user",
        content: "continue",
        createdAt: "2026-06-07T00:00:02.000Z",
        status: "complete",
      },
      activeRun: {
        runId: "run-1",
        status: "input-queued",
        waitingFor: "user_input",
      },
    } as never);
  });

  it("rejects local-code deferred input without workspace authority", async () => {
    mockGetChatHydrated.mockResolvedValue(activeChat() as never);
    const { POST } = await import("@/app/api/chats/[chatId]/input/route");

    const response = await POST(
      postInput({ content: "review ~/github/astra" }),
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(409);
    await expect(response.json()).resolves.toEqual({
      code: "workspace_required",
      error: expect.stringContaining("Select a connected edge workspace"),
    });
    expect(mockQueueDeferredRunInput).not.toHaveBeenCalled();
  });

  it("rejects local-code deferred input on server sandbox", async () => {
    mockGetChatHydrated.mockResolvedValue(
      activeChat({ kind: "server_sandbox" }) as never,
    );
    const { POST } = await import("@/app/api/chats/[chatId]/input/route");

    const response = await POST(
      postInput({ content: "git status and run tests" }),
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(409);
    await expect(response.json()).resolves.toEqual({
      code: "workspace_local_code_on_server_sandbox",
      error: expect.stringContaining("Server sandbox cannot access"),
    });
    expect(mockQueueDeferredRunInput).not.toHaveBeenCalled();
  });

  it("rejects attempts to change workspace with deferred input", async () => {
    mockGetChatHydrated.mockResolvedValue(
      activeChat({
        kind: "edge_workspace",
        edgeAgentId: "edge-a",
        displayName: "MacBook",
        cwd: "/Users/xupeng/github/astra",
      }) as never,
    );
    const { POST } = await import("@/app/api/chats/[chatId]/input/route");

    const response = await POST(
      postInput({
        content: "continue",
        workspace: { kind: "server_sandbox" },
      }),
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(409);
    await expect(response.json()).resolves.toEqual({
      code: "workspace_active_run_immutable",
      error: expect.stringContaining("cannot be changed with deferred input"),
    });
    expect(mockQueueDeferredRunInput).not.toHaveBeenCalled();
  });

  it("accepts non-code deferred input without forcing a workspace", async () => {
    mockGetChatHydrated.mockResolvedValue(activeChat() as never);
    const { POST } = await import("@/app/api/chats/[chatId]/input/route");

    const response = await POST(postInput({ content: "continue" }), {
      params: Promise.resolve({ chatId: "chat-1" }),
    });

    expect(response.status).toBe(200);
    await expect(response.json()).resolves.toEqual(
      expect.objectContaining({
        activeRun: expect.objectContaining({
          runId: "run-1",
          status: "input-queued",
        }),
      }),
    );
    expect(mockQueueDeferredRunInput).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({ content: "continue" }),
    );
  });
});

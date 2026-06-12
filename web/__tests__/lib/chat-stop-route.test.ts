/**
 * @jest-environment node
 */

jest.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: jest.fn(),
}));

jest.mock("@/lib/api/web-store", () => ({
  getChat: jest.fn(),
  stopActiveRun: jest.fn(),
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
import { getChat, stopActiveRun } from "@/lib/api/web-store";

const mockRequireRuntimeUser = requireRuntimeUser as jest.MockedFunction<
  typeof requireRuntimeUser
>;
const mockGetChat = getChat as jest.MockedFunction<typeof getChat>;
const mockStopActiveRun = stopActiveRun as jest.MockedFunction<
  typeof stopActiveRun
>;

describe("chat stop route", () => {
  beforeEach(() => {
    mockRequireRuntimeUser.mockReset();
    mockGetChat.mockReset();
    mockStopActiveRun.mockReset();
  });

  it("uses a bounded runtime cancel wait while keeping a cancelling active run", async () => {
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
    } as never);
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-stop",
        title: "Stop test",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [],
      session: {
        chatId: "chat-stop",
        backendSessionId: "chat-stop",
        persisted: true,
        messageCount: 0,
      },
      activeRun: {
        runId: "run-stop",
        status: "running",
        waitingFor: null,
      },
      workspaceSelectionExplicit: false,
    } as never);
    mockStopActiveRun.mockResolvedValue({
      activeRun: {
        runId: "run-stop",
        status: "cancelling",
        waitingFor: "cancel_requested",
      },
      cancelPending: true,
    });

    const { POST } = await import("@/app/api/chats/[chatId]/stop/route");
    const response = await POST(new Request("http://test.local"), {
      params: Promise.resolve({ chatId: "chat-stop" }),
    });

    expect(response.status).toBe(200);
    expect(mockGetChat).toHaveBeenCalledWith("user-a", "chat-stop");
    expect(mockStopActiveRun).toHaveBeenCalledWith("user-a", "chat-stop", {
      skipSync: true,
      cancelTimeoutMs: 1500,
    });
    await expect(response.json()).resolves.toEqual({
      activeRun: {
        runId: "run-stop",
        status: "cancelling",
        waitingFor: "cancel_requested",
      },
      cancelPending: true,
    });
  });
});

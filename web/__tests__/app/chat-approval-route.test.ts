// @vitest-environment node

vi.mock("@/lib/api/auth-guard", () => ({ requireRuntimeUser: vi.fn() }));
vi.mock("@/lib/api/web-store", () => ({ getChat: vi.fn() }));
vi.mock("@/lib/runtime-client", () => ({
  RuntimeClientError: class RuntimeClientError extends Error {
    status?: number;
    detail: string;
    constructor({ status, detail }: { status?: number; detail: string }) {
      super(detail);
      this.status = status;
      this.detail = detail;
    }
  },
  requireRuntimeClient: vi.fn(),
}));

import { POST } from "@/app/api/chats/[chatId]/approval/route";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";

const mockRequireRuntimeUser = vi.mocked(requireRuntimeUser);
const mockGetChat = vi.mocked(getChat);
const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

function approvalRequest(overrides: Record<string, unknown> = {}) {
  return new Request("http://localhost/api/chats/chat-1/approval", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      requestId: "review-1",
      tool: "exit_plan_mode",
      sessionId: "session-1",
      runId: "run-1",
      decision: "allow",
      approvalKind: "standard",
      ...overrides,
    }),
  });
}

describe("chat approval route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-1" },
      response: null,
    } as Awaited<ReturnType<typeof requireRuntimeUser>>);
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Plan",
        projectId: null,
        createdAt: "2026-08-04T00:00:00Z",
        updatedAt: "2026-08-04T00:00:00Z",
      },
      messages: [],
      session: {
        chatId: "chat-1",
        backendSessionId: "session-1",
        persisted: true,
        messageCount: 0,
      },
      activeRun: { runId: "run-1", status: "waiting" },
    });
  });

  it("forwards an owned active-run decision to the durable runtime interaction", async () => {
    const post = vi.fn().mockResolvedValue(undefined);
    mockRequireRuntimeClient.mockResolvedValue({ post } as never);

    const response = await POST(approvalRequest(), {
      params: Promise.resolve({ chatId: "chat-1" }),
    });

    expect(response.status).toBe(200);
    expect(post).toHaveBeenCalledWith(
      "/approval/respond",
      expect.objectContaining({
        request_id: "review-1",
        decision: "allow",
        session_id: "session-1",
        run_id: "run-1",
        tool_name: "exit_plan_mode",
      }),
      expect.objectContaining({ auth: "required" }),
    );
  });

  it("rejects stale or cross-chat run identity before contacting runtime", async () => {
    const response = await POST(approvalRequest({ runId: "other-run" }), {
      params: Promise.resolve({ chatId: "chat-1" }),
    });

    expect(response.status).toBe(409);
    expect(mockRequireRuntimeClient).not.toHaveBeenCalled();
  });
});

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
import { AstraApiError } from "@astra/sdk";

const mockRequireRuntimeUser = vi.mocked(requireRuntimeUser);
const mockGetChat = vi.mocked(getChat);
const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);
function setAdmittedRuntime(value: unknown) {
  mockRequireRuntimeClient.mockResolvedValue(value as never);
  const previous = mockRequireRuntimeUser.getMockImplementation();
  mockRequireRuntimeUser.mockImplementation(async () => ({ ...(await previous!()), client: value }) as never);
}

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
    setAdmittedRuntime({ post } as never);

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

  it("accepts an approval from a descendant of the active run", async () => {
    const post = vi.fn().mockResolvedValue(undefined);
    const getRunStatus = vi.fn().mockResolvedValue({
      runId: "child-run", sessionId: "session-1", parentRunId: "intermediate-run",
      rootRunId: "run-1", status: "waiting",
    });
    setAdmittedRuntime({ post, sdk: { getRunStatus } });
    const response = await POST(approvalRequest({ runId: "child-run", tool: "bash", approvalKind: "protected_bash_write" }), {
      params: Promise.resolve({ chatId: "chat-1" }),
    });
    expect(response.status).toBe(200);
    expect(getRunStatus).toHaveBeenCalledWith("child-run");
    expect(post).toHaveBeenCalledWith("/approval/respond", expect.objectContaining({ run_id: "child-run", approval_kind: "protected_bash_write" }), expect.anything());
  });

  it("answers a child of the selected settled root without reviving the root", async () => {
    mockGetChat.mockReturnValue({chat: {id: "chat-1"}, session: {backendSessionId: "session-1"}, messages: []} as never);
    const post = vi.fn().mockResolvedValue(undefined);
    const getRunStatus = vi.fn().mockResolvedValue({runId: "child-run", sessionId: "session-1", parentRunId: "root", rootRunId: "root"});
    const getSessionConversation = vi.fn().mockResolvedValue({selected_root: {run_id: "root", status: "cancelled"}});
    setAdmittedRuntime({post, sdk: {getRunStatus, getSessionConversation}});
    const response = await POST(approvalRequest({runId: "child-run"}), {params: Promise.resolve({chatId: "chat-1"})});
    expect(response.status).toBe(200);
    expect(post).toHaveBeenCalledWith("/approval/respond", expect.objectContaining({run_id: "child-run"}), expect.anything());
  });

  it("rejects a settled root's child after a successor is selected", async () => {
    mockGetChat.mockReturnValue({chat: {id: "chat-1"}, session: {backendSessionId: "session-1"}, messages: []} as never);
    const post = vi.fn();
    setAdmittedRuntime({post, sdk: {
      getRunStatus: vi.fn().mockResolvedValue({runId: "child-run", sessionId: "session-1", parentRunId: "old", rootRunId: "old"}),
      getSessionConversation: vi.fn().mockResolvedValue({selected_root: {run_id: "new"}}),
    }});
    const response = await POST(approvalRequest({runId: "child-run"}), {params: Promise.resolve({chatId: "chat-1"})});
    expect(response.status).toBe(409);
    expect(post).not.toHaveBeenCalled();
  });

  it.each([
    { runId: "child-run", sessionId: "session-1", parentRunId: "parent", rootRunId: "other-root" },
    { runId: "child-run", sessionId: "other-session", parentRunId: "parent", rootRunId: "run-1" },
    { runId: "child-run", sessionId: "session-1", parentRunId: null, rootRunId: "run-1" },
    { runId: "other-run", sessionId: "session-1", parentRunId: "parent", rootRunId: "run-1" },
  ])("rejects a child outside the active run or session", async (status) => {
    const post = vi.fn();
    setAdmittedRuntime({ post, sdk: { getRunStatus: vi.fn().mockResolvedValue(status) } });
    const response = await POST(approvalRequest({ runId: "child-run" }), {
      params: Promise.resolve({ chatId: "chat-1" }),
    });
    expect(response.status).toBe(409);
    expect(post).not.toHaveBeenCalled();
  });

  it.each([[404, 409], [503, 503]])("preserves a failed descendant lookup as an actionable error", async (runtimeStatus, expectedStatus) => {
    const post = vi.fn();
    setAdmittedRuntime({ post, sdk: { getRunStatus: vi.fn().mockRejectedValue(new AstraApiError(runtimeStatus, "private runtime detail", "/run")) } });
    const response = await POST(approvalRequest({ runId: "child-run" }), { params: Promise.resolve({ chatId: "chat-1" }) });
    expect(response.status).toBe(expectedStatus);
    expect(JSON.stringify(await response.json())).not.toContain("private runtime detail");
    expect(post).not.toHaveBeenCalled();
  });

  it("forwards a protected write as a one-time scoped approval", async () => {
    const post = vi.fn().mockResolvedValue(undefined);
    setAdmittedRuntime({ post } as never);

    const response = await POST(approvalRequest({
      tool: "bash",
      approvalKind: "protected_bash_write",
    }), { params: Promise.resolve({ chatId: "chat-1" }) });

    expect(response.status).toBe(200);
    expect(post).toHaveBeenCalledWith(
      "/approval/respond",
      expect.objectContaining({ decision: "allow", approval_kind: "protected_bash_write" }),
      expect.anything(),
    );
  });
});

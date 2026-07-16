// @vitest-environment node

vi.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: vi.fn(),
}));

vi.mock("@/lib/api/web-store", () => ({
  getChat: vi.fn(),
}));

vi.mock("@/lib/runtime-client", () => ({
  requireRuntimeClient: vi.fn(),
  runtimeErrorDetail: (error: unknown) =>
    error instanceof Error ? error.message : String(error),
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";

const mockRequireRuntimeUser = vi.mocked(requireRuntimeUser);
const mockGetChat = vi.mocked(getChat);
const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

describe("chat insights route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-1" },
      response: null,
    } as never);
  });

  it("returns useful partial evidence when one projection is unavailable", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Review",
        projectId: null,
        createdAt: "2026-07-16T00:00:00.000Z",
        updatedAt: "2026-07-16T00:00:00.000Z",
      },
      session: {
        chatId: "chat-1",
        backendSessionId: "session-1",
        persisted: true,
        messageCount: 4,
      },
      messages: [],
    });
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getSessionAudit: vi.fn().mockRejectedValue(new Error("audit busy")),
        getSessionReflect: vi.fn().mockResolvedValue({
          session_id: "session-1",
          focus: "auto",
          overview: {},
          diagnoses: [],
          insights: [],
          recommendations: ["Continue with the failing integration test."],
        }),
        getSessionDecisionTrace: vi.fn().mockResolvedValue({
          session_id: "session-1",
          focus: "tool_surface",
          overview: { route: "server sandbox" },
          diagnoses: [],
          insights: [],
          recommendations: [],
        }),
      },
    } as never);

    const { GET } = await import("@/app/api/chats/[chatId]/insights/route");
    const response = await GET(
      new Request("http://web.test/api/chats/chat-1/insights") as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );
    const payload = await response.json();

    expect(response.status).toBe(200);
    expect(payload.audit).toBeNull();
    expect(payload.reflection.recommendations).toEqual([
      "Continue with the failing integration test.",
    ]);
    expect(payload.decisionTrace.overview).toEqual({
      route: "server sandbox",
    });
    expect(payload.warnings).toEqual(["audit: audit busy"]);
  });

  it("explains that insights require a durable session", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "New chat",
        projectId: null,
        createdAt: "2026-07-16T00:00:00.000Z",
        updatedAt: "2026-07-16T00:00:00.000Z",
      },
      session: {
        chatId: "chat-1",
        backendSessionId: null,
        persisted: false,
        messageCount: 1,
      },
      messages: [],
    });

    const { GET } = await import("@/app/api/chats/[chatId]/insights/route");
    const response = await GET(
      new Request("http://web.test/api/chats/chat-1/insights") as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );
    const payload = await response.json();

    expect(response.status).toBe(409);
    expect(payload.code).toBe("session_not_bound");
    expect(mockRequireRuntimeClient).not.toHaveBeenCalled();
  });
});

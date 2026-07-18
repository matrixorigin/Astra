// @vitest-environment node

vi.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: vi.fn(),
}));

vi.mock("@/lib/api/web-store", () => ({
  getChat: vi.fn(),
}));

vi.mock("@/lib/runtime-client", () => ({
  RuntimeClientError: class RuntimeClientError extends Error {
    status: number;

    constructor(message: string, status = 502) {
      super(message);
      this.status = status;
    }
  },
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

describe("chat work surface route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-1" },
      response: null,
    } as never);
  });

  it("rehydrates a completed fanout from the durable run tree after activeRun is cleared", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Review",
        projectId: null,
        createdAt: "2026-07-17T08:00:00Z",
        updatedAt: "2026-07-17T08:05:00Z",
      },
      session: {
        chatId: "chat-1",
        backendSessionId: "session-1",
        persisted: true,
        messageCount: 3,
      },
      messages: [],
    } as never);

    const get = vi.fn(async (path: string) => {
      if (path === "/sessions/session-1/runs?limit=400") {
        return {
          session_id: "session-1",
          truncated: false,
          runs: [
            {
              run_id: "root-new",
              depth: 0,
              status: "cancelled",
              total_tool_calls: 1,
              created_at: "2026-07-17T08:04:00Z",
              updated_at: "2026-07-17T08:05:00Z",
            },
            {
              run_id: "child-a",
              parent_run_id: "root-new",
              root_run_id: "root-new",
              depth: 1,
              agent_id: "security",
              agent_name: "Security review",
              status: "completed",
              total_tool_calls: 4,
              created_at: "2026-07-17T08:04:01Z",
              updated_at: "2026-07-17T08:04:40Z",
            },
            {
              run_id: "child-b",
              parent_run_id: "root-new",
              root_run_id: "root-new",
              depth: 1,
              agent_id: "correctness",
              agent_name: "Correctness review",
              status: "cancelled",
              total_tool_calls: 2,
              created_at: "2026-07-17T08:04:01Z",
              updated_at: "2026-07-17T08:05:00Z",
            },
            {
              run_id: "root-old",
              depth: 0,
              status: "completed",
              total_tool_calls: 0,
              created_at: "2026-07-16T08:00:00Z",
              updated_at: "2026-07-16T08:01:00Z",
            },
          ],
        };
      }
      if (path === "/sessions/session-1/todos") return { tasks: [] };
      if (path.startsWith("/chat/runs/root-new/projection")) {
        return {
          run_id: "root-new",
          session_id: "session-1",
          status: "cancelled",
          recent_events: [
            {
              type: "agent_spawned",
              agent_id: "security",
              run_id: "child-a",
              description: "Inspect auth boundaries",
            },
          ],
        };
      }
      throw new Error(`unexpected runtime path: ${path}`);
    });
    mockRequireRuntimeClient.mockResolvedValue({ get } as never);

    const { GET } = await import(
      "@/app/api/chats/[chatId]/work-surface/route"
    );
    const response = await GET(
      new Request("http://web.test/api/chats/chat-1/work-surface"),
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );
    const payload = await response.json();

    expect(response.status).toBe(200);
    expect(payload).toMatchObject({
      sessionId: "session-1",
      runId: "root-new",
      status: "cancelled",
      warnings: [],
    });
    expect(payload.events).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          type: "agent_projection",
          agent_id: "security",
          run_id: "child-a",
          status: "completed",
          source: "durable_run_tree",
        }),
        expect.objectContaining({
          type: "agent_projection",
          agent_id: "correctness",
          run_id: "child-b",
          status: "cancelled",
          source: "durable_run_tree",
        }),
      ]),
    );
    expect(get).toHaveBeenCalledWith(
      "/chat/runs/root-new/projection?recent_limit=400",
      expect.any(Object),
    );
    expect(
      get.mock.calls.some(([path]) => String(path).includes("root-old/projection")),
    ).toBe(false);
  });

  it("projects a running root and child without fabricating lifecycle events", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Review",
        projectId: null,
        createdAt: "2026-07-17T08:00:00Z",
        updatedAt: "2026-07-17T08:05:00Z",
      },
      session: {
        chatId: "chat-1",
        backendSessionId: "session-1",
        persisted: true,
        messageCount: 2,
      },
      messages: [],
      activeRun: { runId: "root-live", status: "running" },
    } as never);

    const get = vi.fn(async (path: string) => {
      if (path === "/sessions/session-1/runs?limit=400") {
        return {
          session_id: "session-1",
          truncated: false,
          runs: [
            {
              run_id: "root-live",
              depth: 0,
              status: "running",
              total_tool_calls: 1,
              created_at: "2026-07-17T08:04:00Z",
              updated_at: "2026-07-17T08:05:00Z",
            },
            {
              run_id: "child-live",
              parent_run_id: "root-live",
              root_run_id: "root-live",
              depth: 1,
              agent_id: "reviewer",
              agent_name: "Reviewer",
              status: "running",
              total_tool_calls: 2,
              created_at: "2026-07-17T08:04:01Z",
              updated_at: "2026-07-17T08:04:30Z",
            },
          ],
        };
      }
      if (path === "/sessions/session-1/todos") return { tasks: [] };
      if (path.startsWith("/chat/runs/root-live/projection")) {
        return {
          run_id: "root-live",
          session_id: "session-1",
          status: "running",
          workspace: {
            kind: "server_sandbox",
            cwd: "/tmp/workspace",
          },
          executor: {
            kind: "server_local",
            executor_id: "server-local",
            transport: "server_local",
            status: "online",
          },
          recent_events: [
            { type: "text_delta", content: "not surface state" },
            {
              type: "agent_live_gap",
              run_id: "child-live",
              agent_id: "reviewer",
              dropped_event_count: 3,
              repair: "refresh_run_snapshot",
            },
          ],
        };
      }
      throw new Error(`unexpected runtime path: ${path}`);
    });
    mockRequireRuntimeClient.mockResolvedValue({ get } as never);

    const { GET } = await import(
      "@/app/api/chats/[chatId]/work-surface/route"
    );
    const response = await GET(
      new Request("http://web.test/api/chats/chat-1/work-surface"),
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );
    const payload = await response.json();

    expect(response.status).toBe(200);
    expect(payload.status).toBe("running");
    expect(payload.events).toEqual([
      expect.objectContaining({
        type: "binding_projection",
        source: "durable_run_projection",
        status: "running",
      }),
      expect.objectContaining({
        type: "agent_live_gap",
        agent_id: "reviewer",
        dropped_event_count: 3,
      }),
      expect.objectContaining({
        type: "agent_projection",
        source: "durable_run_tree",
        agent_id: "reviewer",
        status: "running",
      }),
    ]);
    expect(payload.events).not.toEqual(
      expect.arrayContaining([
        expect.objectContaining({ type: "run_started" }),
        expect.objectContaining({ type: "agent_spawned" }),
      ]),
    );
  });
});

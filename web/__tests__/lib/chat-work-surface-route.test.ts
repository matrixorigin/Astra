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
function setAdmittedRuntime(value: unknown) {
  mockRequireRuntimeClient.mockResolvedValue(value as never);
  const previous = mockRequireRuntimeUser.getMockImplementation();
  mockRequireRuntimeUser.mockImplementation(async () => ({ ...(await previous!()), client: value }) as never);
}

describe("chat work surface route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-1" },
      response: null,
    } as never);
  });

  it.each([
    ["running", undefined, true, true], ["failed", undefined, true, true], ["delegated", undefined, true, true],
    ["cancelled", "runtime", true, true], ["cancelled", "user", false, true],
    ["cancelled", undefined, false, false], ["completed", undefined, false, true],
  ])("recovers waiting child approvals for root %s (%s)", async (status, origin, expected, complete) => {
    mockGetChat.mockReturnValue({
      chat: { id: "chat-1", title: "Review", projectId: null, createdAt: "", updatedAt: "" },
      session: { chatId: "chat-1", backendSessionId: "session-1", persisted: true, messageCount: 0 },
      messages: [], activeRun: { runId: "root", status },
    } as never);
    const get = vi.fn(async (path: string) => {
      if (path.startsWith("/sessions/")) return { session_id: "session-1", truncated: false, runs: [
        { run_id: "root", depth: 0, status, created_at: "", updated_at: "" },
        { run_id: "child", root_run_id: "root", parent_run_id: "root", depth: 1, status: "waiting", waiting_for: "tool_approval", created_at: "", updated_at: "" },
        { run_id: "other", root_run_id: "other-root", parent_run_id: "other-root", depth: 1, status: "waiting", waiting_for: "tool_approval", created_at: "", updated_at: "" },
      ] };
      if (path.startsWith("/chat/runs/root/projection")) return { run_id: "root", session_id: "session-1", status, cancellation_origin: origin, recent_events: [] };
      if (path === "/chat/runs/root/approvals") return { approvals: [{ type: "approval_required", run_id: "child", session_id: "session-1", request_id: "req", tool: "bash", approval_kind: "protected_bash_write" }], next_cursor: null };
      throw new Error(`unexpected runtime path: ${path}`);
    });
    setAdmittedRuntime({ get, sdk: { getWorkSessionBinding: vi.fn().mockResolvedValue(null) } });
    const { GET } = await import("@/app/api/chats/[chatId]/work-surface/route");
    const response = await GET(new Request("http://web.test"), { params: Promise.resolve({ chatId: "chat-1" }) });
    const payload = await response.json();
    expect(payload.pendingApprovals).toEqual(expected
      ? [expect.objectContaining({ runId: "child", requestId: "req", approvalKind: "protected_bash_write" })] : []);
    expect(payload.pendingApprovalsComplete).toBe(complete);
    expect(get.mock.calls.some(([path]) => path === "/chat/runs/root/approvals")).toBe(expected);
    expect(get.mock.calls.some(([path]) => path === "/chat/runs/other-root/approvals")).toBe(false);
  });

  it("recovers a runtime-cancelled root after the active run is cleared", async () => {
    mockGetChat.mockReturnValue({chat: {id: "chat-1"}, session: {backendSessionId: "session-1"}, messages: []} as never);
    const get = vi.fn(async (path: string) => {
      if (path.startsWith("/sessions/")) return {session_id: "session-1", truncated: false, runs: [
        {run_id: "root", depth: 0, status: "cancelled", created_at: "2026-01-01", updated_at: "2026-01-01"},
        {run_id: "child", parent_run_id: "root", root_run_id: "root", depth: 1, status: "waiting", waiting_for: "tool_approval", created_at: "", updated_at: ""},
      ]};
      if (path.startsWith("/chat/runs/root/projection")) return {run_id: "root", session_id: "session-1", status: "cancelled", cancellation_origin: "runtime", recent_events: []};
      if (path === "/chat/runs/root/approvals" || path === "/chat/runs/selected/approvals") return {approvals: [{type: "approval_required", run_id: "child", session_id: "session-1", request_id: "req", tool: "bash"}], next_cursor: null};
      throw new Error(path);
    });
    setAdmittedRuntime({get, sdk: {
      getWorkSessionBinding: vi.fn().mockResolvedValue(null),
      getSessionConversation: vi.fn().mockResolvedValue({selected_root: {run_id: "root"}}),
    }});
    const {GET} = await import("@/app/api/chats/[chatId]/work-surface/route");
    const response = await GET(new Request("http://web.test"), {params: Promise.resolve({chatId: "chat-1"})});
    expect(await response.json()).toMatchObject({runId: "root", pendingApprovalsComplete: true, pendingApprovals: [{runId: "child", requestId: "req"}]});
  });

  it("recovers a cancelled root from a truncated history with one batch per page", async () => {
    mockGetChat.mockReturnValue({chat: {id: "chat-1"}, session: {backendSessionId: "session-1"}, messages: []} as never);
    const approvals = Array.from({length: 100}, (_, index) => ({
      type: "approval_required", run_id: `child-${index}`, session_id: "session-1",
      request_id: `request-${index}`, tool: "bash",
    }));
    const get = vi.fn(async (path: string) => {
      if (path.startsWith("/sessions/")) return {session_id: "session-1", truncated: true, runs: [
        {run_id: "child-0", parent_run_id: "root", root_run_id: "root", depth: 1, agent_id: "agent-0", status: "waiting", waiting_for: "tool_approval", created_at: "", updated_at: ""},
      ]};
      if (path.startsWith("/chat/runs/root/projection")) return {run_id: "root", session_id: "session-1", status: "cancelled", cancellation_origin: "runtime", recent_events: []};
      if (path === "/chat/runs/root/approvals") return {approvals, next_cursor: null};
      throw new Error(path);
    });
    setAdmittedRuntime({get, sdk: {
      getSessionConversation: vi.fn().mockResolvedValue({selected_root: {run_id: "root"}}),
      getWorkSessionBinding: vi.fn().mockResolvedValue(null),
    }});
    const {GET} = await import("@/app/api/chats/[chatId]/work-surface/route");
    const response = await GET(new Request("http://web.test"), {params: Promise.resolve({chatId: "chat-1"})});
    const payload = await response.json();
    expect(payload.pendingApprovalsComplete).toBe(true);
    expect(payload.pendingApprovals).toHaveLength(100);
    expect(payload.events).toContainEqual(expect.objectContaining({run_id: "child-0", status: "waiting", reason: "tool_approval"}));
    expect(get.mock.calls.filter(([path]) => path.includes("/approvals"))).toHaveLength(1);
  });

  it("keeps a partial approval page retryable when the next page fails", async () => {
    mockGetChat.mockReturnValue({chat: {id: "chat-1"}, session: {backendSessionId: "session-1"}, messages: [], activeRun: {runId: "root"}} as never);
    const get = vi.fn(async (path: string) => {
      if (path.startsWith("/sessions/")) return {session_id: "session-1", truncated: true, runs: [
        {run_id: "root", depth: 0, status: "running", created_at: "", updated_at: ""},
      ]};
      if (path.startsWith("/chat/runs/root/projection")) return {run_id: "root", session_id: "session-1", status: "running", recent_events: []};
      if (path === "/chat/runs/root/approvals") return {approvals: [{type: "approval_required", run_id: "child", session_id: "session-1", request_id: "req", tool: "bash"}], next_cursor: "next/page"};
      if (path === "/chat/runs/root/approvals?cursor=next%2Fpage") throw new Error("second page unavailable");
      throw new Error(path);
    });
    setAdmittedRuntime({get, sdk: {getWorkSessionBinding: vi.fn().mockResolvedValue(null)}});
    const {GET} = await import("@/app/api/chats/[chatId]/work-surface/route");
    const payload = await (await GET(new Request("http://web.test"), {params: Promise.resolve({chatId: "chat-1"})})).json();
    expect(payload).toMatchObject({pendingApprovalsComplete: false, pendingApprovals: [{runId: "child"}], warnings: [expect.any(String), expect.stringContaining("second page unavailable")]});
  });

  it.each([true, false])("uses the conversation's selected root for settled approval recovery (available: %s)", async available => {
    mockGetChat.mockReturnValue({chat: {id: "chat-1"}, session: {backendSessionId: "session-1"}, messages: []} as never);
    const get = vi.fn(async (path: string) => {
      if (path.startsWith("/sessions/")) return {session_id: "session-1", truncated: false, runs: [
        {run_id: "selected", depth: 0, status: "cancelled", created_at: "2026-01-01", updated_at: "2026-01-01"},
        {run_id: "newer", depth: 0, status: "completed", created_at: "2026-01-02", updated_at: "2026-01-02"},
        {run_id: "child", parent_run_id: "selected", root_run_id: "selected", depth: 1, status: "waiting", waiting_for: "tool_approval", created_at: "", updated_at: ""},
      ]};
      if (path.startsWith("/chat/runs/selected/projection")) return {run_id: "selected", session_id: "session-1", status: "cancelled", cancellation_origin: "runtime", recent_events: []};
      if (path.startsWith("/chat/runs/newer/projection")) return {run_id: "newer", session_id: "session-1", status: "completed", recent_events: []};
      if (path === "/chat/runs/root/approvals" || path === "/chat/runs/selected/approvals") return {approvals: [{type: "approval_required", run_id: "child", session_id: "session-1", request_id: "req", tool: "bash"}], next_cursor: null};
      throw new Error(path);
    });
    const getSessionConversation = available
      ? vi.fn().mockResolvedValue({selected_root: {run_id: "selected"}})
      : vi.fn().mockRejectedValue(new Error("conversation unavailable"));
    setAdmittedRuntime({get, sdk: {getSessionConversation, getWorkSessionBinding: vi.fn().mockResolvedValue(null)}});
    const {GET} = await import("@/app/api/chats/[chatId]/work-surface/route");
    const response = await GET(new Request("http://web.test"), {params: Promise.resolve({chatId: "chat-1"})});
    const payload = await response.json();
    if (available) {
      expect(payload).toMatchObject({runId: "selected", pendingApprovalsComplete: true, pendingApprovals: [{runId: "child"}]});
    } else {
      expect(payload).toMatchObject({pendingApprovalsComplete: false, pendingApprovals: [], warnings: [expect.stringContaining("conversation unavailable")]});
      expect(get.mock.calls.some(([path]) => path === "/chat/runs/selected/approvals")).toBe(false);
    }
    expect(getSessionConversation).toHaveBeenCalledWith("session-1", 1);
  });

  it.each(["projection failure", "frontier failure", "invalid frontier"])(
    "does not declare approval recovery complete on %s", async failure => {
      mockGetChat.mockReturnValue({
        chat: { id: "chat-1" }, session: { backendSessionId: "session-1" },
        messages: [], activeRun: { runId: "root", status: "running" },
      } as never);
      const get = vi.fn(async (path: string) => {
        if (path.startsWith("/sessions/")) {
          return { session_id: "session-1", truncated: false, runs: [
            { run_id: "root", depth: 0, status: failure === "projection failure" ? "completed" : "running", created_at: "", updated_at: "" },
            { run_id: "child", root_run_id: "root", parent_run_id: "root", depth: 1, status: "waiting", waiting_for: "tool_approval", created_at: "", updated_at: "" },
          ] };
        }
        if (path.startsWith("/chat/runs/root/projection")) {
          if (failure === "projection failure") throw new Error("projection unavailable");
          return { run_id: "root", session_id: "session-1", status: "running", recent_events: [] };
        }
        if (path === "/chat/runs/root/approvals") {
          if (failure === "frontier failure") throw new Error("frontier unavailable");
          if (failure === "invalid frontier") return { approvals: [{ type: "approval_required" }], next_cursor: null };
          return { approvals: [], next_cursor: null };
        }
        throw new Error(`unexpected runtime path: ${path}`);
      });
      setAdmittedRuntime({ get, sdk: { getWorkSessionBinding: vi.fn().mockResolvedValue(null) } });
      const { GET } = await import("@/app/api/chats/[chatId]/work-surface/route");
      const response = await GET(new Request("http://web.test"), { params: Promise.resolve({ chatId: "chat-1" }) });
      expect((await response.json()).pendingApprovalsComplete).toBe(false);
      expect(get.mock.calls.some(([path]) => path === "/chat/runs/root/approvals")).toBe(failure !== "projection failure");
    },
  );

  it("recovers approvals when the display tree is unavailable", async () => {
    mockGetChat.mockReturnValue({chat: {id: "chat-1"}, session: {backendSessionId: "session-1"}, messages: [], activeRun: {runId: "root"}} as never);
    const get = vi.fn(async (path: string) => {
      if (path.startsWith("/sessions/")) throw new Error("tree unavailable");
      if (path.startsWith("/chat/runs/root/projection")) return {run_id: "root", session_id: "session-1", status: "running", recent_events: []};
      if (path === "/chat/runs/root/approvals") return {approvals: [{type: "approval_required", run_id: "child", session_id: "session-1", request_id: "req", tool: "bash"}], next_cursor: null};
      throw new Error(path);
    });
    setAdmittedRuntime({get, sdk: {getWorkSessionBinding: vi.fn().mockResolvedValue(null)}});
    const {GET} = await import("@/app/api/chats/[chatId]/work-surface/route");
    const payload = await (await GET(new Request("http://web.test"), {params: Promise.resolve({chatId: "chat-1"})})).json();
    expect(payload).toMatchObject({pendingApprovalsComplete: true, pendingApprovals: [{runId: "child", requestId: "req"}]});
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
    const workBinding = {
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-1",
      graph_revision: 4,
    };
    const taskGraph = { schema_version: 2, scope: "declared_work" };
    const getWorkSessionBinding = vi.fn().mockResolvedValue(workBinding);
    const getWorkTaskGraph = vi.fn().mockResolvedValue(taskGraph);
    setAdmittedRuntime({
      get,
      sdk: { getWorkSessionBinding, getWorkTaskGraph },
    } as never);

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
      workBinding,
      taskGraph,
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
    expect(get.mock.calls.some(([path]) => String(path).includes("/todos"))).toBe(false);
    expect(getWorkTaskGraph).toHaveBeenCalledWith("work-1", "branch-1", {
      cursor: { graph_revision: 4, item_offset: 0, dependency_offset: 0 },
      itemLimit: 8,
      dependencyLimit: 128,
    });
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
    const getWorkSessionBinding = vi.fn().mockResolvedValue(null);
    setAdmittedRuntime({
      get,
      sdk: { getWorkSessionBinding },
    } as never);

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
        total_tool_calls: 2,
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

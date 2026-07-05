// @vitest-environment node

vi.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: vi.fn(),
}));

vi.mock("@/lib/api/web-store", () => ({
  archiveChat: vi.fn(),
  deleteChat: vi.fn(),
  getChat: vi.fn(),
  getChatHydrated: vi.fn(),
  moveChat: vi.fn(),
  updateChatModel: vi.fn(),
  updateChatWorkspaceSelection: vi.fn(),
}));

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

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  getChat,
  updateChatWorkspaceSelection,
} from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";

const mockRequireRuntimeUser = vi.mocked(requireRuntimeUser);
const mockGetChat = vi.mocked(getChat);
const mockUpdateChatWorkspaceSelection = vi.mocked(
  updateChatWorkspaceSelection,
);
const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

const connectedEdge = {
  edge_agent_id: "edge-1",
  hostname: "MacBook Pro",
  workspace_dir: "/Users/test/astra",
  connected_secs: 8,
};

function runtimeWithEdges(edges: Array<Record<string, unknown>> = [connectedEdge]) {
  return {
    get: vi.fn().mockResolvedValue({ edges }),
  };
}

function chatDetail(workspaceSelection?: unknown) {
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
      messageCount: 0,
    },
    messages: [],
    workspaceSelection,
    workspaceSelectionExplicit: Boolean(workspaceSelection),
  };
}

function patchChat(body: Record<string, unknown>) {
  return new Request("http://web.test/api/chats/chat-1", {
    method: "PATCH",
    body: JSON.stringify(body),
  }) as never;
}

describe("chat route workspace selection patch", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
      response: null,
    } as never);
    mockGetChat.mockReturnValue(chatDetail() as never);
    mockUpdateChatWorkspaceSelection.mockImplementation(
      async (_ownerUserId, _chatId, selection) =>
        chatDetail(selection ?? undefined) as never,
    );
    mockRequireRuntimeClient.mockResolvedValue(runtimeWithEdges() as never);
  });

  it("persists a valid edge workspace selection", async () => {
    const { PATCH } = await import("@/app/api/chats/[chatId]/route");
    const workspaceSelection = {
      kind: "edge_workspace",
      edgeAgentId: "edge-1",
      displayName: "MacBook Pro",
      cwd: "/Users/test/astra",
    };

    const response = await PATCH(patchChat({ workspaceSelection }), {
      params: Promise.resolve({ chatId: "chat-1" }),
    });

    expect(response.status).toBe(200);
    expect(mockUpdateChatWorkspaceSelection).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      workspaceSelection,
    );
  });

  it("rejects an edge workspace selection that is not visible for the current user", async () => {
    const { PATCH } = await import("@/app/api/chats/[chatId]/route");
    mockRequireRuntimeClient.mockResolvedValue(runtimeWithEdges([]) as never);

    const response = await PATCH(
      patchChat({
        workspaceSelection: {
          kind: "edge_workspace",
          edgeAgentId: "edge-1",
          displayName: "MacBook Pro",
          cwd: "/Users/test/astra",
        },
      }),
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(409);
    expect(mockUpdateChatWorkspaceSelection).not.toHaveBeenCalled();
  });

  it("clears a workspace selection when workspaceSelection is null", async () => {
    const { PATCH } = await import("@/app/api/chats/[chatId]/route");

    const response = await PATCH(patchChat({ workspaceSelection: null }), {
      params: Promise.resolve({ chatId: "chat-1" }),
    });

    expect(response.status).toBe(200);
    expect(mockUpdateChatWorkspaceSelection).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      null,
    );
  });

  it("rejects invalid workspace selections", async () => {
    const { PATCH } = await import("@/app/api/chats/[chatId]/route");

    const response = await PATCH(
      patchChat({ workspaceSelection: { kind: "edge_workspace" } }),
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(400);
    expect(mockUpdateChatWorkspaceSelection).not.toHaveBeenCalled();
  });
});

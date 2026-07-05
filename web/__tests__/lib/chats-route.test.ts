// @vitest-environment node

vi.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: vi.fn(),
}));

vi.mock("@/lib/api/web-store", () => ({
  createChatWithMessage: vi.fn(),
  deleteArchivedChats: vi.fn(),
  listChats: vi.fn(),
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
import { createChatWithMessage } from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";

const mockRequireRuntimeUser = vi.mocked(requireRuntimeUser);
const mockCreateChatWithMessage = vi.mocked(createChatWithMessage);
const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

function postChats(body: Record<string, unknown>) {
  return new Request("http://web.test/api/chats", {
    method: "POST",
    body: JSON.stringify(body),
  }) as never;
}

function runtimeWithEdges(edges: Array<Record<string, unknown>>) {
  return {
    get: vi.fn().mockResolvedValue({ edges }),
  };
}

describe("chats route create workspace selection", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
      response: null,
    } as never);
    mockCreateChatWithMessage.mockResolvedValue({
      chatId: "chat-1",
      messageId: "message-1",
    });
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeWithEdges([
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 7,
        },
      ]) as never,
    );
  });

  it("verifies and persists an initial edge workspace selection", async () => {
    const { POST } = await import("@/app/api/chats/route");

    const response = await POST(
      postChats({
        message: "list files",
        model: "sonnet-4.6-adaptive",
        options: {
          webSearch: false,
          thinking: true,
          activeSkills: [],
        },
        projectId: null,
        workspaceSelection: {
          kind: "edge_workspace",
          edgeAgentId: "edge-1",
          displayName: "MacBook Pro",
          cwd: "/Users/test/astra",
        },
      }),
    );

    expect(response.status).toBe(201);
    expect(mockCreateChatWithMessage).toHaveBeenCalledWith(
      "user-a",
      expect.objectContaining({
        workspaceSelection: {
          kind: "edge_workspace",
          edgeAgentId: "edge-1",
          displayName: "MacBook Pro",
          cwd: "/Users/test/astra",
        },
      }),
    );
  });

  it("rejects an invalid initial workspace selection", async () => {
    const { POST } = await import("@/app/api/chats/route");

    const response = await POST(
      postChats({
        message: "list files",
        model: "sonnet-4.6-adaptive",
        options: {
          webSearch: false,
          thinking: true,
          activeSkills: [],
        },
        workspaceSelection: { kind: "edge_workspace" },
      }),
    );

    expect(response.status).toBe(400);
    expect(mockCreateChatWithMessage).not.toHaveBeenCalled();
  });
});

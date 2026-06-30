// @vitest-environment node

vi.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: vi.fn(),
}));

vi.mock("@/lib/api/web-store", () => ({
  beginStreamingMessage: vi.fn(),
  ensureChatBackendSession: vi.fn(),
  getChat: vi.fn(),
  resolveBackendModelName: vi.fn(),
  selectedWebModel: vi.fn((model?: string | null) => {
    const normalized = model?.trim();
    return normalized || "sonnet-4.6-adaptive";
  }),
  setChatActiveRun: vi.fn(),
  updateChatWorkspaceSelection: vi.fn(),
  updateStreamingAssistantMessage: vi.fn(),
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
  WebRuntimeClient: class WebRuntimeClient {},
  readRuntimeErrorDetail: vi.fn(),
  requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  beginStreamingMessage,
  ensureChatBackendSession,
  getChat,
  resolveBackendModelName,
  setChatActiveRun,
  updateChatWorkspaceSelection,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";
import { PATH_EDGES_STATUS } from "@astra/sdk";

const mockRequireRuntimeUser = vi.mocked(requireRuntimeUser);
const mockGetChat = vi.mocked(getChat);
const mockResolveBackendModelName = vi.mocked(resolveBackendModelName);
const mockBeginStreamingMessage = vi.mocked(beginStreamingMessage);
const mockEnsureChatBackendSession = vi.mocked(ensureChatBackendSession);
const mockSetChatActiveRun = vi.mocked(setChatActiveRun);
const mockUpdateChatWorkspaceSelection = vi.mocked(
  updateChatWorkspaceSelection,
);
const mockUpdateStreamingAssistantMessage = vi.mocked(
  updateStreamingAssistantMessage,
);
const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

function makeBackendStream() {
  let releasePendingRead: (() => void) | null = null;
  const cancel = vi.fn(() => {
    releasePendingRead?.();
    return Promise.resolve();
  });
  const releaseLock = vi.fn();
  const read = vi.fn(
    () =>
      new Promise<{ value?: Uint8Array; done: boolean }>((resolve) => {
        releasePendingRead = () => resolve({ value: undefined, done: true });
      }),
  );

  return {
    body: {
      getReader: () => ({
        read,
        cancel,
        releaseLock,
      }),
    },
    cancel,
    releaseLock,
  };
}

function makeBackendFrameStream(frames: string[]) {
  const encoder = new TextEncoder();
  const chunks = frames.map((frame) => encoder.encode(frame));
  let index = 0;
  const releaseLock = vi.fn();
  const cancel = vi.fn();

  return {
    body: {
      getReader: () => ({
        async read() {
          if (index >= chunks.length) {
            return { value: undefined, done: true };
          }
          const value = chunks[index];
          index += 1;
          return { value, done: false };
        },
        cancel,
        releaseLock,
      }),
    },
    cancel,
    releaseLock,
  };
}

function waitForStreamWork(ms = 0) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitUntil(predicate: () => boolean) {
  for (let index = 0; index < 20; index += 1) {
    if (predicate()) {
      return;
    }
    await waitForStreamWork(0);
  }
  throw new Error("condition was not met");
}

const connectedEdge = {
  edge_agent_id: "edge-1",
  hostname: "MacBook Pro",
  workspace_dir: "/Users/xupeng/github/astra",
  connected_secs: 12,
};

function makeRuntimeWithEdgeStatus(
  backend: ReturnType<typeof makeBackendStream>,
  edges: Array<Record<string, unknown>> = [connectedEdge],
) {
  return {
    sdk: {
      getRuntimeSession: vi.fn().mockResolvedValue({}),
      listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
    },
    get: vi.fn().mockResolvedValue({ edges }),
    fetchResponse: vi.fn().mockResolvedValue({
      ok: true,
      body: backend.body,
    }),
  };
}

describe("chat stream route proxy cancellation", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
      response: null,
    } as never);
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [],
    });
    mockResolveBackendModelName.mockResolvedValue({
      id: "model-backend",
      model: "backend-model",
    });
    mockEnsureChatBackendSession.mockResolvedValue("chat-1");
    mockUpdateChatWorkspaceSelection.mockResolvedValue({
      chat: {
        id: "chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [],
      workspaceSelection: { kind: "server_sandbox" },
    });
    mockBeginStreamingMessage.mockReturnValue({
      userMessage: {
        id: "user-1",
        role: "user",
        content: "hello",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "complete",
      },
      assistantMessage: {
        id: "assistant-1",
        role: "assistant",
        content: "",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "streaming",
      },
      sessionId: "chat-1",
    });
  });

  it("cancels the backend SSE reader when the web client disconnects", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = {
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    await reader?.read();
    await waitForStreamWork();
    await reader?.cancel();

    const signal = runtime.fetchResponse.mock.calls[0]?.[1]?.signal as
      | AbortSignal
      | undefined;
    expect(signal).toBeDefined();
    expect(signal?.aborted).toBe(true);
    expect(backend.cancel).toHaveBeenCalled();
    expect(backend.releaseLock).toHaveBeenCalled();
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalledWith(
      expect.anything(),
      expect.anything(),
      expect.anything(),
      expect.objectContaining({ status: "failed" }),
    );
  });

  it("persists a cancelled backend run as a clean stopped assistant message", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"run_started","run_id":"run-stop"}\n\n',
      'data: {"type":"run_finished","run_id":"run-stop","status":"cancelled"}\n\n',
    ]);
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    } as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    for (;;) {
      const { done } = await reader!.read();
      if (done) {
        break;
      }
    }

    expect(mockSetChatActiveRun).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      undefined,
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "Stopped.",
        status: "complete",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalledWith(
      expect.anything(),
      expect.anything(),
      expect.anything(),
      expect.objectContaining({ status: "failed" }),
    );
  });

  it("persists blocked backend events as visible non-terminal feedback", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"run_started","run_id":"run-blocked"}\n\n',
      'data: {"type":"run_blocked","session_id":"chat-1","reason":"executor_offline","message":"Edge executor MacBook Pro is offline."}\n\n',
    ]);
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    } as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    for (;;) {
      const { done } = await reader!.read();
      if (done) {
        break;
      }
    }

    expect(mockSetChatActiveRun).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        runId: "run-blocked",
        status: "blocked",
        waitingFor: "executor_offline",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "Edge executor MacBook Pro is offline.",
        status: "streaming",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalledWith(
      expect.anything(),
      expect.anything(),
      expect.anything(),
      expect.objectContaining({ status: "failed" }),
    );
  });

  it("returns local SSE messages before the backend stream connection resolves", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    let resolveFetch: (value: {
      ok: boolean;
      body: ReturnType<typeof makeBackendStream>["body"];
    }) => void = () => {};
    const fetchResponse = vi.fn(
      (_path: string, init: { signal?: AbortSignal }) =>
        new Promise<{
          ok: boolean;
          body: ReturnType<typeof makeBackendStream>["body"];
        }>((resolve) => {
          resolveFetch = resolve;
        }),
    );
    const runtime = {
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse,
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    const first = await reader!.read();
    const text = new TextDecoder().decode(first.value);
    expect(text).toContain('"type":"local_messages"');
    await vi.waitFor(() => expect(fetchResponse).toHaveBeenCalledTimes(1));

    const backend = makeBackendStream();
    resolveFetch({ ok: true, body: backend.body });
    await reader?.cancel();
  });

  it("resumes an existing run from cursor into the requested assistant message", async () => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"text_done","full_text":"resumed output","index":9}\n\n',
    ]);
    const runtime = {
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);
    mockGetChat.mockReturnValue({
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
        backendSessionId: "runtime-session-1",
        persisted: true,
        messageCount: 3,
      },
      messages: [
        {
          id: "assistant-old",
          role: "assistant",
          content: "old",
          createdAt: "2026-06-07T00:00:00.000Z",
          status: "complete",
        },
        {
          id: "assistant-queued",
          role: "assistant",
          content: "",
          createdAt: "2026-06-07T00:00:01.000Z",
          status: "streaming",
        },
      ],
      activeRun: {
        runId: "run-1",
        status: "input-queued",
        waitingFor: "user_input",
        assistantMessageId: "assistant-queued",
        nextEventIndex: 9,
      },
    } as never);

    const url = new URL(
      "http://web.test/api/chats/chat-1/stream?runId=run-1&last_index=9&assistantMessageId=assistant-queued",
    );
    const response = await GET({ nextUrl: url } as never, {
      params: Promise.resolve({ chatId: "chat-1" }),
    });
    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    for (;;) {
      const { done } = await reader!.read();
      if (done) {
        break;
      }
    }

    expect(runtime.fetchResponse).toHaveBeenCalledWith(
      expect.stringContaining("/runs/run-1/stream?last_index=9"),
      expect.objectContaining({ method: "GET" }),
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-queued",
      expect.objectContaining({ content: "resumed output" }),
    );
  });

  it("returns an existing-run SSE response before the backend stream connection resolves", async () => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    let fetchResolved = false;
    let resolveFetch: (value: {
      ok: boolean;
      body: ReturnType<typeof makeBackendStream>["body"];
    }) => void = () => {};
    const fetchResponse = vi.fn(
      (_path: string, _init: { signal?: AbortSignal }) =>
        new Promise<{
          ok: boolean;
          body: ReturnType<typeof makeBackendStream>["body"];
        }>((resolve) => {
          resolveFetch = (value) => {
            fetchResolved = true;
            resolve(value);
          };
        }),
    );
    const runtime = {
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse,
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);
    mockGetChat.mockReturnValue({
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
        backendSessionId: "runtime-session-1",
        persisted: true,
        messageCount: 2,
      },
      messages: [
        {
          id: "assistant-queued",
          role: "assistant",
          content: "",
          createdAt: "2026-06-07T00:00:01.000Z",
          status: "streaming",
        },
      ],
      activeRun: {
        runId: "run-1",
        status: "running",
        waitingFor: null,
        assistantMessageId: "assistant-queued",
      },
    } as never);

    const url = new URL("http://web.test/api/chats/chat-1/stream?runId=run-1");
    const response = await GET({ nextUrl: url } as never, {
      params: Promise.resolve({ chatId: "chat-1" }),
    });

    expect(response.status).toBe(200);
    expect(fetchResponse).toHaveBeenCalledTimes(1);
    expect(fetchResolved).toBe(false);
    const signal = fetchResponse.mock.calls[0]?.[1]?.signal as
      | AbortSignal
      | undefined;
    expect(signal?.aborted).toBe(false);

    const reader = response.body?.getReader();
    await reader?.cancel();
    expect(signal?.aborted).toBe(true);
    resolveFetch({ ok: true, body: makeBackendStream().body });
  });

  it("rejects local code prompts without workspace authority before creating stream messages", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "review ~/github/astra",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(409);
    await expect(response.json()).resolves.toEqual({
      code: "workspace_required",
      error: expect.stringContaining("Select a connected edge workspace"),
    });
    expect(mockBeginStreamingMessage).not.toHaveBeenCalled();
    expect(mockRequireRuntimeClient).not.toHaveBeenCalled();
  });

  it("rejects local code prompts when server sandbox is explicitly selected", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "多角度 review 这个分支的 changes",
          workspace: { kind: "server_sandbox" },
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(409);
    await expect(response.json()).resolves.toEqual({
      code: "workspace_local_code_on_server_sandbox",
      error: expect.stringContaining("Server sandbox cannot access"),
    });
    expect(mockBeginStreamingMessage).not.toHaveBeenCalled();
    expect(mockRequireRuntimeClient).not.toHaveBeenCalled();
  });

  it("forwards selected edge workspace bindings after returning local SSE", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = makeRuntimeWithEdgeStatus(backend);
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "review /Users/xupeng/github/astra/src/lib.rs",
          workspace: {
            kind: "edge_workspace",
            edgeAgentId: "edge-1",
            displayName: "MacBook Pro",
            cwd: "/Users/xupeng/github/astra",
          },
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: ["rust"],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    const first = await reader!.read();
    const text = new TextDecoder().decode(first.value);
    expect(text).toContain('"type":"local_messages"');

    await waitUntil(() => runtime.fetchResponse.mock.calls.length > 0);
    const fetchCalls = runtime.fetchResponse.mock.calls as unknown as Array<
      [unknown, { json?: Record<string, unknown> }]
    >;
    expect(fetchCalls[0]?.[1].json).toEqual(
      expect.objectContaining({
        parts: [],
        attachments: [],
        selected_model: { id: "model-backend", model: "backend-model" },
        workspace_binding: {
          kind: "edge_workspace",
          display_name: "MacBook Pro",
          cwd: "/Users/xupeng/github/astra",
          authority: "read_write",
          fallback_policy: "disabled",
        },
        executor_binding: {
          kind: "edge_agent",
          executor_id: "edge-1",
          display_name: "MacBook Pro",
          transport: "edge_ws",
          status: "online",
        },
        context: expect.objectContaining({
          edge_profile: {
            cwd: "/Users/xupeng/github/astra",
            edge_agent_id: "edge-1",
            active_skills: ["rust"],
          },
        }),
      }),
    );
    expect(runtime.get).toHaveBeenCalledWith(PATH_EDGES_STATUS, {
      auth: "required",
      operation: "verify edge workspace binding",
    });

    await reader?.cancel();
  });

  it("streams an error after local SSE when the selected edge workspace is offline", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = makeRuntimeWithEdgeStatus(backend, []);
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "review /Users/xupeng/github/astra/src/lib.rs",
          workspace: {
            kind: "edge_workspace",
            edgeAgentId: "edge-1",
            displayName: "MacBook Pro",
            cwd: "/Users/xupeng/github/astra",
          },
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(200);
    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    const first = await reader!.read();
    expect(new TextDecoder().decode(first.value)).toContain(
      '"type":"local_messages"',
    );
    const second = await reader!.read();
    const secondText = new TextDecoder().decode(second.value);
    expect(secondText).toContain('"type":"error"');
    expect(secondText).toContain("Server fallback is disabled");
    expect(runtime.fetchResponse).not.toHaveBeenCalled();
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: expect.stringContaining("Server fallback is disabled"),
        status: "failed",
      }),
    );
  });

  it("returns local SSE messages before backend session creation and model resolution finish", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    mockEnsureChatBackendSession.mockImplementation(
      () => new Promise(() => {}),
    );
    mockResolveBackendModelName.mockImplementation(() => new Promise(() => {}));
    const runtime = {
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn(),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    const first = await reader!.read();
    const text = new TextDecoder().decode(first.value);
    expect(text).toContain('"type":"local_messages"');
    expect(mockEnsureChatBackendSession).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({ runtime }),
    );
    await reader?.cancel();
  });

  it("returns local SSE messages before the runtime client is ready", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    mockRequireRuntimeClient.mockImplementation(() => new Promise(() => {}));

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    const first = await reader!.read();
    const text = new TextDecoder().decode(first.value);
    expect(text).toContain('"type":"local_messages"');
    expect(mockRequireRuntimeClient).toHaveBeenCalledWith({
      auth: "required",
      operation: "stream web chat turn",
    });
    await reader?.cancel();
  });

  it("recovers an empty first stream request from the pending turn", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [
        {
          id: "pending-user-1",
          role: "user" as const,
          content: "hello from pending",
          createdAt: "2026-06-07T00:00:00.000Z",
          status: "complete" as const,
        },
      ],
      pendingTurn: {
        messageId: "pending-user-1",
        content: "hello from pending",
        options: {
          model: "sonnet-4.6-adaptive",
          webSearch: false,
          thinking: true,
          activeSkills: [],
        },
      },
    });
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    } as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(200);
    expect(mockBeginStreamingMessage).toHaveBeenCalledWith("user-a", "chat-1", {
      content: "hello from pending",
      pendingMessageId: "pending-user-1",
      options: {
        model: "sonnet-4.6-adaptive",
        webSearch: false,
        thinking: true,
        activeSkills: [],
      },
    });
    const reader = response.body?.getReader();
    await reader?.read();
    await reader?.cancel();
  });

  it("inherits chat workspace selection when recovering an empty pending turn request", async () => {
    const edgeWorkspace = {
      kind: "edge_workspace" as const,
      edgeAgentId: "edge-1",
      displayName: "MacBook Pro",
      cwd: "/Users/xupeng/github/astra",
    };
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [
        {
          id: "pending-user-1",
          role: "user" as const,
          content: "review this repo",
          createdAt: "2026-06-07T00:00:00.000Z",
          status: "complete" as const,
        },
      ],
      pendingTurn: {
        messageId: "pending-user-1",
        content: "review this repo",
        options: {
          model: "sonnet-4.6-adaptive",
          webSearch: false,
          thinking: true,
          activeSkills: [],
        },
      },
      workspaceSelection: edgeWorkspace,
    });
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = makeRuntimeWithEdgeStatus(backend);
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(200);
    expect(mockUpdateChatWorkspaceSelection).not.toHaveBeenCalled();
    expect(mockBeginStreamingMessage).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        workspaceSelection: edgeWorkspace,
      }),
    );

    const reader = response.body?.getReader();
    await reader?.read();
    await waitUntil(() => runtime.fetchResponse.mock.calls.length > 0);
    const fetchCalls = runtime.fetchResponse.mock.calls as unknown as Array<
      [unknown, { json?: Record<string, unknown> }]
    >;
    expect(fetchCalls[0]?.[1].json).toEqual(
      expect.objectContaining({
        parts: [],
        attachments: [],
        selected_model: { id: "model-backend", model: "backend-model" },
        workspace_binding: expect.objectContaining({
          kind: "edge_workspace",
          cwd: "/Users/xupeng/github/astra",
        }),
        executor_binding: expect.objectContaining({
          kind: "edge_agent",
          executor_id: "edge-1",
        }),
      }),
    );
    await reader?.cancel();
  });

  it("rejects malformed stream request JSON without crashing", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: "{",
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    await expect(response.json()).resolves.toEqual({
      error: "invalid request body",
    });
    expect(response.status).toBe(400);
    expect(mockBeginStreamingMessage).not.toHaveBeenCalled();
  });
});

describe("chat stream route artifact fetch optimization", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
      response: null,
    } as never);
    mockResolveBackendModelName.mockResolvedValue({
      id: "model-backend",
      model: "backend-model",
    });
    mockEnsureChatBackendSession.mockResolvedValue("chat-1");
    mockBeginStreamingMessage.mockReturnValue({
      userMessage: {
        id: "user-1",
        role: "user",
        content: "hello",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "complete",
      },
      assistantMessage: {
        id: "assistant-1",
        role: "assistant",
        content: "",
        createdAt: "2026-06-07T00:00:00.000Z",
        status: "streaming",
      },
      sessionId: "chat-1",
    });
  });

  it("skips fetchSessionArtifacts for new chats (no prior messages)", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [], // New chat — no messages
    });

    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const listSessionArtifacts = vi.fn().mockResolvedValue({ artifacts: [] });
    const runtime = {
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts,
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(listSessionArtifacts).not.toHaveBeenCalled();
  });

  it("skips fetchSessionArtifacts for new chats with only a pending first user message", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [
        {
          id: "pending-user-1",
          role: "user" as const,
          content: "hello",
          createdAt: "2026-06-07T00:00:00.000Z",
          status: "complete" as const,
        },
      ],
      pendingTurn: {
        messageId: "pending-user-1",
        content: "hello",
        options: {
          model: "sonnet-4.6-adaptive",
          webSearch: false,
          thinking: true,
          activeSkills: [],
        },
      },
    });

    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const listSessionArtifacts = vi.fn().mockResolvedValue({ artifacts: [] });
    const runtime = {
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts,
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          pendingMessageId: "pending-user-1",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(listSessionArtifacts).not.toHaveBeenCalled();
  });

  it("fetches artifacts for chats with prior messages", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [
        {
          id: "msg-1",
          role: "user" as const,
          content: "previous message",
          createdAt: "2026-06-06T00:00:00.000Z",
          status: "complete" as const,
        },
      ],
    });

    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const listSessionArtifacts = vi.fn().mockResolvedValue({ artifacts: [] });
    const runtime = {
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts,
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    await reader?.read();
    await waitForStreamWork();
    await reader?.cancel();

    expect(listSessionArtifacts).toHaveBeenCalledTimes(1);
  });

  it("creates the backend session and resolves the model in parallel", async () => {
    mockGetChat.mockReturnValue({
      chat: {
        id: "chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      messages: [],
    });

    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();

    // Track call order to verify they run concurrently
    const callOrder: string[] = [];
    mockEnsureChatBackendSession.mockImplementation(async () => {
      callOrder.push("session-start");
      // Small delay to simulate network
      await new Promise((r) => setTimeout(r, 10));
      callOrder.push("session-end");
      return "chat-1";
    });

    // Override resolveBackendModelName to track timing
    mockResolveBackendModelName.mockImplementation(async () => {
      callOrder.push("model-start");
      await new Promise((r) => setTimeout(r, 10));
      callOrder.push("model-end");
      return { id: "model-backend", model: "backend-model" };
    });

    const runtime = {
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "hello",
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    const reader = response.body?.getReader();
    await reader?.read();
    await waitForStreamWork(20);
    await reader?.cancel();

    // Both calls should have been made
    expect(mockEnsureChatBackendSession).toHaveBeenCalledTimes(1);
    expect(mockResolveBackendModelName).toHaveBeenCalledTimes(1);

    // They started concurrently: both "start" before either "end"
    const sessionStartIdx = callOrder.indexOf("session-start");
    const modelStartIdx = callOrder.indexOf("model-start");
    const sessionEndIdx = callOrder.indexOf("session-end");
    const modelEndIdx = callOrder.indexOf("model-end");

    expect(sessionStartIdx).toBeLessThan(sessionEndIdx);
    expect(modelStartIdx).toBeLessThan(modelEndIdx);
    // Concurrent: both started before either finished
    const firstEnd = Math.min(sessionEndIdx, modelEndIdx);
    expect(sessionStartIdx).toBeLessThan(firstEnd);
    expect(modelStartIdx).toBeLessThan(firstEnd);
  });
});

describe("chat existing run stream route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
      response: null,
    } as never);
    mockGetChat.mockReturnValue({
      chat: {
        id: "web-chat-1",
        title: "Chat",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        updatedAt: "2026-06-07T00:00:00.000Z",
        archivedAt: null,
        model: "sonnet-4.6-adaptive",
      },
      session: {
        chatId: "web-chat-1",
        backendSessionId: "runtime-session-1",
        persisted: true,
        messageCount: 2,
      },
      messages: [
        {
          id: "user-1",
          role: "user" as const,
          content: "hello",
          createdAt: "2026-06-07T00:00:00.000Z",
          status: "complete" as const,
        },
        {
          id: "assistant-1",
          role: "assistant" as const,
          content: "",
          createdAt: "2026-06-07T00:00:01.000Z",
          status: "streaming" as const,
        },
      ],
    });
  });

  it("uses the backend session id when reconnecting an existing run stream", async () => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"session_info","session_id":"runtime-session-1","run_id":"run-1"}\n\n',
      'data: {"type":"text_delta","content":"reply"}\n\n',
      'data: {"type":"run_finished","run_id":"run-1","status":"completed"}\n\n',
    ]);
    const listSessionArtifacts = vi.fn().mockResolvedValue({ artifacts: [] });
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        listSessionArtifacts,
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    } as never);

    const request = new Request(
      "http://web.test/api/chats/web-chat-1/stream?runId=run-1",
      {
        method: "GET",
      },
    );
    Object.defineProperty(request, "nextUrl", {
      value: new URL("http://web.test/api/chats/web-chat-1/stream?runId=run-1"),
    });

    const response = await GET(request as never, {
      params: Promise.resolve({ chatId: "web-chat-1" }),
    });

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    for (;;) {
      const { done } = await reader!.read();
      if (done) {
        break;
      }
    }

    expect(listSessionArtifacts).toHaveBeenCalledWith("runtime-session-1", {
      limit: 50,
    });
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalledWith(
      "user-a",
      "web-chat-1",
      "assistant-1",
      expect.objectContaining({
        content: expect.stringContaining("Web chat is bound to web-chat-1"),
        status: "failed",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "web-chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "reply",
        status: "complete",
      }),
    );
  });

  it("clears the active run when the backend stream reports a mismatched session id", async () => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"session_info","session_id":"wrong-session","run_id":"run-1"}\n\n',
    ]);
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    } as never);
    const request = new Request(
      "http://web.test/api/chats/web-chat-1/stream?runId=run-1",
      { method: "GET" },
    );
    Object.defineProperty(request, "nextUrl", {
      value: new URL("http://web.test/api/chats/web-chat-1/stream?runId=run-1"),
    });

    const response = await GET(request as never, {
      params: Promise.resolve({ chatId: "web-chat-1" }),
    });

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    for (;;) {
      const { done } = await reader!.read();
      if (done) {
        break;
      }
    }

    expect(mockSetChatActiveRun).toHaveBeenCalledWith(
      "user-a",
      "web-chat-1",
      undefined,
    );
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
      "user-a",
      "web-chat-1",
      "assistant-1",
      expect.objectContaining({
        content: expect.stringContaining("wrong-session"),
        status: "failed",
      }),
    );
  });
});

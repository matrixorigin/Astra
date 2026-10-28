// @vitest-environment node

vi.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: vi.fn(),
}));

vi.mock("@/lib/api/web-store", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api/web-store")>();
  return {
  normalizedActiveSkills: actual.normalizedActiveSkills,
  normalizedActiveTools: actual.normalizedActiveTools,
  claimChatStreamProjection: vi.fn(),
  withChatStreamProjection: vi.fn((_lease, apply) => { apply(); return true; }),
  releaseChatStreamProjection: vi.fn(),
  ChatStreamProjectionError: class extends Error {},
  beginStreamingMessage: vi.fn(),
  ensureChatBackendSession: vi.fn(),
  getChat: vi.fn(),
  resolveModelOfferingSelection: vi.fn(),
  requireSelectedOfferingId: vi.fn((model?: string | null) => {
    const normalized = model?.trim();
    if (!normalized) {
      throw new Error("model offering required");
    }
    return normalized;
  }),
  setChatActiveRun: vi.fn(),
  updateChatWorkspaceSelection: vi.fn(),
  updateStreamingAssistantMessage: vi.fn(),
  };
});

vi.mock("@/lib/runtime-client", () => ({
  RuntimeClientError: class RuntimeClientError extends Error {
    status?: number;
    detail: string;
    code?: string;

    constructor({
      status,
      detail,
      code,
    }: {
      status?: number;
      detail: string;
      code?: string;
    }) {
      super(detail);
      this.status = status;
      this.detail = detail;
      this.code = code;
    }
  },
  WebRuntimeClient: class WebRuntimeClient {},
  readRuntimeErrorDetail: vi.fn(),
  requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  beginStreamingMessage,
  claimChatStreamProjection,
  ensureChatBackendSession,
  getChat,
  resolveModelOfferingSelection,
  setChatActiveRun,
  updateChatWorkspaceSelection,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";
import { PATH_EDGES_STATUS } from "@astra/sdk";
import workspaceBindingContract from "../../../fixtures/contracts/workspace_binding_request.json";

// Independent server fixture: this intentionally does not import the SDK
// constant, so a stale Web major cannot make the route tests self-consistent.
const SERVER_INTERACTION_API_MAJOR_HEADER =
  "x-astra-agent-interaction-api-major";
const SERVER_INTERACTION_API_MAJOR = "3";

const mockClaimProjection = vi.mocked(claimChatStreamProjection);
beforeEach(() => {
  mockClaimProjection.mockImplementation((ownerUserId, chatId, assistantMessageId, writer, binding) => ({
    ownerUserId, chatId, writer, projection: { assistantMessageId, sessionId: binding.sessionId, writer,
      state: { assistantText: "", assistantRawText: "", reasoningText: "", lastStatus: "streaming",
        protocolError: false, runLifecycle: "running", runId: binding.runId, nextEventIndex: binding.nextEventIndex ?? 0 } },
  }));
});

const mockRequireRuntimeUser = vi.mocked(requireRuntimeUser);
const mockGetChat = vi.mocked(getChat);
const mockResolveModelOfferingSelection = vi.mocked(resolveModelOfferingSelection);
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
function setAdmittedRuntime(value: unknown) {
  mockRequireRuntimeClient.mockResolvedValue(value as never);
  const previous = mockRequireRuntimeUser.getMockImplementation();
  mockRequireRuntimeUser.mockImplementation(async () => ({ ...(await previous!()), client: value }) as never);
}

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
    headers: new Headers({
      [SERVER_INTERACTION_API_MAJOR_HEADER]: SERVER_INTERACTION_API_MAJOR,
    }),
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

function makeBackendFrameStream(frames: string[], readError?: Error) {
  const encoder = new TextEncoder();
  const chunks = frames.map((frame) => encoder.encode(frame));
  let index = 0;
  const releaseLock = vi.fn();
  const cancel = vi.fn();

  return {
    headers: new Headers({
      [SERVER_INTERACTION_API_MAJOR_HEADER]: SERVER_INTERACTION_API_MAJOR,
    }),
    body: {
      getReader: () => ({
        async read() {
          if (index >= chunks.length) {
            if (readError) throw readError;
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
  await vi.waitFor(
    () => {
      if (!predicate()) {
        throw new Error("condition was not met");
      }
    },
    { interval: 5, timeout: 1000 },
  );
}

const connectedEdge = {
  edge_agent_id: "edge-1",
  hostname: "MacBook Pro",
  workspace_dir: "/Users/test/astra",
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
        headers: backend.headers,
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
    mockResolveModelOfferingSelection.mockResolvedValue({ offeringId: "model-backend" });
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

  it("uses the canonical browser proof to retry late chat admission without replaying a different turn", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const { WebRuntimeClient } = await vi.importActual<typeof import("@/lib/runtime-client/server")>("@/lib/runtime-client/server");
    const root = "https://runtime.example/";
    const handle = `bs3.${Buffer.from(root).toString("base64url")}.${Buffer.from("user-id").toString("base64url")}.${Buffer.from("session-id").toString("base64url")}.${Buffer.alloc(32, 1).toString("base64url")}`;
    const runtime = new WebRuntimeClient({ mode: "live", source: "cookie", apiUrl: root,
      accessToken: "old", refreshToken: handle, hasAccessToken: true, hasRefreshToken: true,
      demoMode: false, message: "" });
    setAdmittedRuntime(runtime);
    const admissions: Array<{ body: string; bearer: string | null }> = [];
    const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = new URL(String(input)).pathname;
      if (path === "/auth/refresh") {
        expect(JSON.parse(String(init?.body)).refresh_token).toBe(handle);
        return Response.json({ access_token: "current", refresh_token: handle });
      }
      if (path === "/chat/stream") {
        admissions.push({ body: String(init?.body), bearer: new Headers(init?.headers).get("authorization") });
        if (admissions.length === 1) return new Response("expired", { status: 401 });
        return new Response('data: {"type":"content","content":"Hello"}\n\ndata: {"type":"done"}\n\n', {
          headers: { "content-type": "text/event-stream", [SERVER_INTERACTION_API_MAJOR_HEADER]: SERVER_INTERACTION_API_MAJOR },
        });
      }
      return Response.json({ artifacts: [], edges: [] });
    });
    vi.stubGlobal("fetch", fetch);
    try {
      const response = await POST(new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST", body: JSON.stringify({ content: "hello", options: { model: "sonnet-4.6-adaptive", activeSkills: [] } }),
      }) as never, { params: Promise.resolve({ chatId: "chat-1" }) });
      expect(response.status).toBe(200);
      await response.text();
      expect(admissions).toHaveLength(2);
      expect(admissions[0].bearer).toBe("Bearer old");
      expect(admissions[1]).toEqual({ body: admissions[0].body, bearer: "Bearer current" });
      expect(JSON.parse(admissions[1].body).session_id).toBe("chat-1");
      expect(response.headers.has("set-cookie")).toBe(false);
    } finally { vi.unstubAllGlobals(); }
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
        headers: backend.headers,
        body: backend.body,
      }),
    };
    setAdmittedRuntime(runtime as never);

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

  it("fails before consuming a stale server stream without the interaction contract", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    backend.headers = new Headers();
    const runtime = {
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
        body: backend.body,
      }),
    };
    setAdmittedRuntime(runtime as never);

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

    const body = await response.text();
    expect(body).toContain("RUNTIME_PROTOCOL_MISMATCH");
    expect(body).toContain("missing the interaction protocol contract");
    expect(runtime.fetchResponse.mock.calls[0]?.[1]?.signal.aborted).toBe(true);
    expect(backend.body.getReader().read).not.toHaveBeenCalled();
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({ status: "failed" }),
    );
  });

  it("rejects a server interaction major from the previous contract", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    backend.headers = new Headers({
      [SERVER_INTERACTION_API_MAJOR_HEADER]: "1",
    });
    const runtime = {
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
        body: backend.body,
      }),
    };
    setAdmittedRuntime(runtime as never);

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

    const body = await response.text();
    expect(body).toContain("RUNTIME_PROTOCOL_MISMATCH");
    expect(body).toContain(
      "Astra Server interaction protocol 1 is incompatible with Web 3",
    );
    expect(runtime.fetchResponse.mock.calls[0]?.[1]?.signal.aborted).toBe(true);
    expect(backend.body.getReader().read).not.toHaveBeenCalled();
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({ status: "failed" }),
    );
  });

  it("persists a cancelled backend run as a clean stopped assistant message", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"run_started","run_id":"run-stop"}\n\n',
      'data: {"type":"run_finished","run_id":"run-stop","status":"cancelled"}\n\n',
    ]);
    setAdmittedRuntime({
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
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
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
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
    setAdmittedRuntime({
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
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
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
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

  it("rejects explicit paths outside the selected edge workspace before opening a backend run", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "review /Users/test/other/src/lib.rs",
          workspace: {
            kind: "edge_workspace",
            edgeAgentId: "edge-1",
            displayName: "MacBook Pro",
            cwd: "/Users/test/astra",
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

    await expect(response.json()).resolves.toEqual({
      code: "workspace_path_mismatch",
      error:
        "The referenced path is outside the selected file environment: /Users/test/other/src/lib.rs. Choose the environment that contains it or use a path inside the current one.",
    });
    expect(response.status).toBe(409);
    expect(mockBeginStreamingMessage).not.toHaveBeenCalled();
    expect(mockRequireRuntimeClient).not.toHaveBeenCalled();
  });

  it("rejects explicit host paths on server sandbox before opening a backend run", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "read /etc/passwd",
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

    await expect(response.json()).resolves.toEqual({
      code: "workspace_path_mismatch",
      error:
        "The selected Server sandbox is an isolated workspace and cannot access host path: /etc/passwd. Use a relative path inside the sandbox, or select an Edge workspace rooted at that host path.",
    });
    expect(response.status).toBe(409);
    expect(mockBeginStreamingMessage).not.toHaveBeenCalled();
    expect(mockRequireRuntimeClient).not.toHaveBeenCalled();
  });

  it("ignores malformed event indexes without aborting an otherwise valid backend stream", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"run_started","run_id":"run-soft","index":"bad"}\n\n',
      'data: {"type":"text_done","full_text":"done"}\n\n',
      'data: {"type":"run_finished","run_id":"run-soft","status":"completed"}\n\n',
    ]);
    setAdmittedRuntime({
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
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

    expect(backend.cancel).not.toHaveBeenCalled();
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "done",
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

  it("returns local SSE messages before the backend stream connection resolves", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    let resolveFetch: (value: {
      ok: boolean;
      headers: Headers;
      body: ReturnType<typeof makeBackendStream>["body"];
    }) => void = () => {};
    const fetchResponse = vi.fn(
      (_path: string, init: { signal?: AbortSignal }) =>
        new Promise<{
          ok: boolean;
          headers: Headers;
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
    setAdmittedRuntime(runtime as never);

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
    resolveFetch({ ok: true, headers: backend.headers, body: backend.body });
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
        headers: backend.headers,
        body: backend.body,
      }),
    };
    setAdmittedRuntime(runtime as never);
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

  it.each([
    {
      label: "with a truncated final frame",
      fromOrigin: true,
      malformed: true,
      frames: [
        `data: ${JSON.stringify({ type: "explain_analyze", schema_version: 1,
          event_id: "clock:1", run_id: "run-1", turn_id: "turn-1", node_id: "turn-1",
          producer_id: "worker", clock_domain_id: "clock", kind: "turn", label: "Answer",
          transition: "started", elapsed_ms: 0 })}\n\n`,
        'data: {"type":"explain_analyze","node_id":',
      ],
    },
    {
      label: "with recovered graph facts and a stale terminal",
      fromOrigin: true,
      frames: [
        `data: ${JSON.stringify({ type: "explain_analyze", schema_version: 1,
          event_id: "clock-1:2", run_id: "run-1", turn_id: "turn-1",
          node_id: "turn-1", producer_id: "worker-1", clock_domain_id: "clock-1",
          kind: "turn", label: "Answer request", transition: "finished",
          elapsed_ms: 30, start_elapsed_ms: 0, duration_ms: 30, outcome: "completed" })}\n\n`,
        'data: {"type":"text_delta","content":"stale text","index":9}\n\n',
        'data: {"type":"error","message":"historical provider failure","index":12}\n\n',
        'data: {"type":"run_finished","run_id":"run-1","status":"failed"}\n\n',
      ],
    },
    {
      label: "with a transcript delta",
      frames: [
        'data: {"type":"stream_gap","run_id":"run-1","dropped_event_count":2,"repair":"replay"}\n\n',
        'data: {"type":"text_delta","content":"suffix","index":9}\n\n',
      ],
    },
    {
      label: "without a transcript delta",
      frames: [
        'data: {"type":"stream_gap","run_id":"run-1","dropped_event_count":2,"repair":"replay"}\n\n',
      ],
    },
  ])(
    "forwards finite replay-only repair requests $label without changing live state",
    async ({ frames, fromOrigin, malformed }) => {
      const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
      const backend = makeBackendFrameStream(frames);
      const runtime = {
        sdk: {
          listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
        },
        fetchResponse: vi.fn().mockResolvedValue({
          ok: true,
          headers: backend.headers,
          body: backend.body,
        }),
      };
      setAdmittedRuntime(runtime as never);
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
            id: "assistant-1",
            role: "assistant",
            content: "already persisted prefix",
            createdAt: "2026-06-07T00:00:01.000Z",
            status: "streaming",
          },
        ],
        activeRun: {
          runId: "run-1",
          status: "running",
          waitingFor: null,
          assistantMessageId: "assistant-1",
          nextEventIndex: 9,
        },
      } as never);

      const request = new Request(
        `http://web.test/api/chats/chat-1/stream?runId=run-1&last_index=${fromOrigin ? 0 : 8}&replay_only=true`,
        { method: "GET" },
      );
      Object.defineProperty(request, "nextUrl", {
        value: new URL(request.url),
      });
      const response = await GET(request as never, {
        params: Promise.resolve({ chatId: "chat-1" }),
      });
      const responseText = await response.text();
      if (malformed) {
        expect(responseText).toContain("Incomplete or malformed replay event");
      } else expect(responseText).toBe(frames.join(""));

      expect(runtime.fetchResponse).toHaveBeenCalledWith(
        `/chat/runs/run-1/stream?last_index=${fromOrigin ? 0 : 8}&replay_only=true`,
        expect.objectContaining({ method: "GET" }),
      );
      expect(mockSetChatActiveRun).not.toHaveBeenCalled();
      const facts = frames.flatMap((frame) => { try { return [JSON.parse(frame.slice(6))]; } catch { return []; } })
        .filter((event) => event.type === "explain_analyze");
      expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledTimes(facts.length + (fromOrigin ? 2 : 0));
      if (fromOrigin) {
        const begin = mockUpdateStreamingAssistantMessage.mock.calls[0]?.[3].explainAnalyzeRepair;
        expect(begin).toEqual({ token: expect.any(String), complete: false });
        expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
          "user-a", "chat-1", "assistant-1", malformed ? { explainAnalyzeDegraded: true }
            : { explainAnalyzeRepair: { token: begin?.token, complete: true } });
      }
      for (const event of facts) {
        expect(mockUpdateStreamingAssistantMessage).toHaveBeenCalledWith(
          "user-a", "chat-1", "assistant-1", { explainAnalyzeEvent: event });
      }
    },
  );

  it("returns an existing-run SSE response before the backend stream connection resolves", async () => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    let fetchResolved = false;
    let resolveFetch: (value: {
      ok: boolean;
      headers: Headers;
      body: ReturnType<typeof makeBackendStream>["body"];
    }) => void = () => {};
    const fetchResponse = vi.fn(
      (_path: string, _init: { signal?: AbortSignal }) =>
        new Promise<{
          ok: boolean;
          headers: Headers;
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
    setAdmittedRuntime(runtime as never);
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
    resolveFetch({
      ok: true,
      headers: new Headers({
        [SERVER_INTERACTION_API_MAJOR_HEADER]: SERVER_INTERACTION_API_MAJOR,
      }),
      body: makeBackendStream().body,
    });
  });

  it("streams prompts without an explicit environment as default Astra turns", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = makeRuntimeWithEdgeStatus(backend);
    setAdmittedRuntime(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "summarize the current plan",
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
    const text = new TextDecoder().decode(first.value);
    expect(text).toContain('"type":"local_messages"');

    await waitUntil(() => runtime.fetchResponse.mock.calls.length > 0);
    const fetchCalls = runtime.fetchResponse.mock.calls as unknown as Array<
      [unknown, { json?: Record<string, unknown> }]
    >;
    expect(fetchCalls[0]?.[1].json).toEqual(
      expect.objectContaining({
        workspace_binding: workspaceBindingContract.none,
        executor_binding: {
          kind: "server_local",
          executor_id: "server-control-plane",
          display_name: "Server control plane",
          transport: "server_local",
          status: "online",
        },
      }),
    );
    expect(runtime.get).not.toHaveBeenCalled();

    await reader?.cancel();
  });

  it("rejects invalid file environment selections before starting a run", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "review the current branch",
          workspace: { kind: "unknown_environment" },
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

    expect(response.status).toBe(400);
    await expect(response.json()).resolves.toEqual({
      error: "workspace must be a valid file environment selection",
      code: "invalid_workspace_selection",
    });
    expect(mockBeginStreamingMessage).not.toHaveBeenCalled();
    expect(mockRequireRuntimeClient).not.toHaveBeenCalled();
  });

  it("streams local-code prompts when server sandbox is explicitly selected", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = makeRuntimeWithEdgeStatus(backend);
    setAdmittedRuntime(runtime as never);

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

    expect(response.status).toBe(200);
    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    await waitUntil(() => runtime.fetchResponse.mock.calls.length > 0);
    const fetchCalls = runtime.fetchResponse.mock.calls as unknown as Array<
      [unknown, { json?: Record<string, unknown> }]
    >;
    expect(fetchCalls[0]?.[1].json).toEqual(
      expect.objectContaining({
        workspace_binding: workspaceBindingContract.server_sandbox,
        executor_binding: {
          kind: "server_local",
          executor_id: "server-local",
          display_name: "Server sandbox",
          transport: "server_local",
          status: "online",
        },
      }),
    );
    expect(runtime.get).not.toHaveBeenCalled();

    await reader?.cancel();
  });

  it("does not rewrite an unchanged explicit workspace selection", async () => {
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
      workspaceSelection: { kind: "server_sandbox" },
    });
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = makeRuntimeWithEdgeStatus(backend);
    setAdmittedRuntime(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "review the current branch",
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

    expect(response.status).toBe(200);
    const reader = response.body?.getReader();
    await waitUntil(() => runtime.fetchResponse.mock.calls.length > 0);
    expect(mockUpdateChatWorkspaceSelection).not.toHaveBeenCalled();
    await reader?.cancel();
  });

  it("forwards workspace bindings and selected connectors after returning local SSE", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = makeRuntimeWithEdgeStatus(backend);
    setAdmittedRuntime(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "review /Users/test/astra/src/lib.rs",
          workspace: {
            kind: "edge_workspace",
            edgeAgentId: "edge-1",
            displayName: "MacBook Pro",
            cwd: "/Users/test/astra",
          },
          options: {
            model: "sonnet-4.6-adaptive",
            webSearch: true,
            thinking: true,
            activeSkills: ["rust"],
            activeTools: ["github"],
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
        model_selection: { offering_id: "model-backend" },
        enabled_tools: ["github", "web_fetch", "web_search"],
        workspace_binding: workspaceBindingContract.edge_workspace,
        executor_binding: {
          kind: "edge_agent",
          executor_id: "edge-1",
          display_name: "MacBook Pro",
          transport: "edge_ws",
          status: "online",
        },
        context: expect.objectContaining({
          edge_profile: {
            cwd: "/Users/test/astra",
            edge_agent_id: "edge-1",
            active_skills: ["rust"],
            active_tools: ["github", "web_fetch", "web_search"],
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
    setAdmittedRuntime(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
        body: JSON.stringify({
          content: "review /Users/test/astra/src/lib.rs",
          workspace: {
            kind: "edge_workspace",
            edgeAgentId: "edge-1",
            displayName: "MacBook Pro",
            cwd: "/Users/test/astra",
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
    expect(secondText).toContain('"code":"workspace_edge_unavailable"');
    expect(secondText).toContain("No alternate execution provider");
    expect(runtime.fetchResponse).not.toHaveBeenCalled();
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: expect.stringContaining("No alternate execution provider"),
        status: "failed",
      }),
    );
  });

  it("returns local SSE messages before backend session creation and model resolution finish", async () => {
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    mockEnsureChatBackendSession.mockImplementation(
      () => new Promise(() => {}),
    );
    mockResolveModelOfferingSelection.mockImplementation(() => new Promise(() => {}));
    const runtime = {
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn(),
    };
    setAdmittedRuntime(runtime as never);

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
    setAdmittedRuntime({
      sdk: {
        getRuntimeSession: vi.fn().mockResolvedValue({}),
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
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
      cwd: "/Users/test/astra",
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
    setAdmittedRuntime(runtime as never);

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
        model_selection: { offering_id: "model-backend" },
        workspace_binding: workspaceBindingContract.edge_workspace,
        executor_binding: expect.objectContaining({
          kind: "edge_agent",
          executor_id: "edge-1",
        }),
      }),
    );
    await reader?.cancel();
  });

  it("rejects a recovered pending turn when its edge provider id is stale", async () => {
    const storedWorkspace = {
      kind: "edge_workspace" as const,
      edgeAgentId: "edge-old-random",
      displayName: "MacBook Pro",
      cwd: "/Users/test/astra",
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
      workspaceSelection: storedWorkspace,
    });
    const { POST } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendStream();
    const runtime = makeRuntimeWithEdgeStatus(backend, [
      {
        edge_agent_id: "edge-new-stable",
        hostname: "MacBook Pro",
        workspace_dir: "/Users/test/astra",
        connected_secs: 1,
      },
    ]);
    setAdmittedRuntime(runtime as never);

    const response = await POST(
      new Request("http://web.test/api/chats/chat-1/stream", {
        method: "POST",
      }) as never,
      { params: Promise.resolve({ chatId: "chat-1" }) },
    );

    expect(response.status).toBe(200);
    expect(mockBeginStreamingMessage).toHaveBeenCalledWith(
      "user-a",
      "chat-1",
      expect.objectContaining({
        workspaceSelection: storedWorkspace,
      }),
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    const first = await reader!.read();
    expect(new TextDecoder().decode(first.value)).toContain(
      '"type":"local_messages"',
    );
    const second = await reader!.read();
    const secondText = new TextDecoder().decode(second.value);
    expect(secondText).toContain('"type":"error"');
    expect(secondText).toContain('"code":"workspace_edge_stale_selection"');
    expect(secondText).toContain("No alternate execution provider");
    expect(runtime.fetchResponse).not.toHaveBeenCalled();
    expect(mockUpdateChatWorkspaceSelection).not.toHaveBeenCalled();
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "chat-1",
      "assistant-1",
      expect.objectContaining({
        content: expect.stringContaining("No alternate execution provider"),
        status: "failed",
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

describe("chat stream route startup", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
      response: null,
    } as never);
    mockResolveModelOfferingSelection.mockResolvedValue({ offeringId: "model-backend" });
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
        headers: backend.headers,
        body: backend.body,
      }),
    };
    setAdmittedRuntime(runtime as never);

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
        headers: backend.headers,
        body: backend.body,
      }),
    };
    setAdmittedRuntime(runtime as never);

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
        headers: backend.headers,
        body: backend.body,
      }),
    };
    setAdmittedRuntime(runtime as never);

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

    // Track Offering admission independently from session resolution.
    mockResolveModelOfferingSelection.mockImplementation(async () => {
      callOrder.push("model-start");
      await new Promise((r) => setTimeout(r, 10));
      callOrder.push("model-end");
      return { offeringId: "model-backend" };
    });

    const runtime = {
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
        body: backend.body,
      }),
    };
    setAdmittedRuntime(runtime as never);

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
    expect(mockResolveModelOfferingSelection).toHaveBeenCalledTimes(1);

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

  it.each([
    { coalesced: false, resumed: false },
    { coalesced: true, resumed: false },
    { coalesced: false, resumed: true },
  ])("preserves partial output through failure (coalesced=$coalesced, cursor=$resumed)", async ({ coalesced, resumed }) => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    if (resumed) {
      const stored = mockGetChat("user-a", "web-chat-1")!.messages.find(message => message.id === "assistant-1")!;
      stored.content = "Useful partial answer";
      stored.reasoning = "Partial reasoning";
      const { applyStreamEvent } = await import("@/lib/api/stream-event-handler");
      const lease = mockClaimProjection("user-a", "web-chat-1", "assistant-1", new AbortController(),
        { sessionId: "runtime-session-1", runId: "root" });
      const ctx = { ownerUserId: "user-a", chatId: "web-chat-1", assistantMessageId: "assistant-1", getSessionId: () => "runtime-session-1" };
      applyStreamEvent({ type: "text_delta", content: stored.content }, ctx, lease.projection.state);
      applyStreamEvent({ type: "reasoning_delta", content: stored.reasoning, index: 7 }, ctx, lease.projection.state);
      mockClaimProjection.mockReturnValue(lease);
    }
    const frames = [
      { type: "session_info", session_id: "runtime-session-1", run_id: "root" },
      { type: "text_delta", content: "Useful partial answer" },
      { type: "reasoning_delta", content: "Partial reasoning" },
      { type: "run_finished", run_id: "child", status: "cancelled" },
      { type: "run_error", run_id: "root", error: "Save failed", error_code: "canonical_persistence_failed" },
      { type: "run_finished", run_id: "root", status: "failed", error: "Save failed" },
    ].filter(event => !resumed || !["text_delta", "reasoning_delta"].includes(event.type))
      .map(event => `data: ${JSON.stringify(event)}\n\n`);
    frames.push("data: [DONE]\n\n");
    const backend = makeBackendFrameStream(coalesced ? [frames.join("")] : frames);
    setAdmittedRuntime({ sdk: { listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }) },
      fetchResponse: vi.fn().mockResolvedValue({ ok: true, headers: backend.headers, body: backend.body }) } as never);
    const url = `http://web.test/api/chats/web-chat-1/stream?assistantMessageId=assistant-1&runId=root${resumed ? "&last_index=8" : ""}`;
    const request = new Request(url, { method: "GET" });
    Object.defineProperty(request, "nextUrl", { value: new URL(url) });
    const response = await GET(request as never, { params: Promise.resolve({ chatId: "web-chat-1" }) });
    const wire = await response.text();
    expect(wire).toContain('"run_id":"child"');
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith("user-a", "web-chat-1", "assistant-1",
      expect.objectContaining({ content: "Useful partial answer\n\nSave failed", reasoning: "Partial reasoning",
        reasoningStatus: undefined, status: "failed" }));
    expect(mockSetChatActiveRun.mock.calls.every(([, , run]) => run?.runId !== "child")).toBe(true);
  });

  it.each([true, false])("preserves the proxy failure cause through a reset (canonical=%s)", async (canonical) => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    const { streamExistingChatRun } = await import("@/lib/api/chats");
    const events = [
      { type: "session_info", session_id: "runtime-session-1", run_id: "root" },
      { type: "text_delta", content: "Useful partial answer" },
      ...(canonical ? [{ type: "run_error", run_id: "root", error: "Save failed",
        error_code: "canonical_persistence_failed" }] : []),
    ];
    const backend = makeBackendFrameStream(events.map(event => `data: ${JSON.stringify(event)}\n\n`), new Error("upstream reset"));
    setAdmittedRuntime({ sdk: { listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }) }, fetchResponse: vi.fn().mockResolvedValue({
      ok: true, headers: backend.headers, body: backend.body,
    }) } as never);
    const url = "http://web.test/api/chats/web-chat-1/stream?assistantMessageId=assistant-1&runId=root";
    const request = new Request(url);
    Object.defineProperty(request, "nextUrl", { value: new URL(url) });
    const response = await GET(request as never, { params: Promise.resolve({ chatId: "web-chat-1" }) });
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue(response);
    const onText = vi.fn(), onDone = vi.fn();
    try {
      await expect(streamExistingChatRun("web-chat-1", "root", { onText, onDone })).rejects.toMatchObject({
        code: canonical ? "canonical_persistence_failed" : "stream_observation_interrupted",
      });
      expect(onText).toHaveBeenLastCalledWith("Useful partial answer");
      expect(onDone).not.toHaveBeenCalled();
      expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith("user-a", "web-chat-1", "assistant-1",
        expect.objectContaining({ content: canonical ? "Useful partial answer\n\nSave failed" : "Useful partial answer", status: canonical ? "failed" : "streaming" }));
      expect(backend.releaseLock).toHaveBeenCalled();
    } finally {
      fetchSpy.mockRestore();
    }
  });

  it("uses the backend session id when reconnecting an existing run stream", async () => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"session_info","session_id":"runtime-session-1","run_id":"run-1"}\n\n',
      'data: {"type":"text_delta","content":"reply"}\n\n',
      'data: {"type":"run_finished","run_id":"run-1","status":"completed"}\n\n',
    ]);
    const listSessionArtifacts = vi.fn().mockResolvedValue({ artifacts: [] });
    setAdmittedRuntime({
      sdk: {
        listSessionArtifacts,
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
        body: backend.body,
      }),
    } as never);

    const request = new Request(
      "http://web.test/api/chats/web-chat-1/stream?assistantMessageId=assistant-1&runId=run-1",
      {
        method: "GET",
      },
    );
    Object.defineProperty(request, "nextUrl", {
      value: new URL("http://web.test/api/chats/web-chat-1/stream?assistantMessageId=assistant-1&runId=run-1"),
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
    setAdmittedRuntime({
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
        body: backend.body,
      }),
    } as never);
    const request = new Request(
      "http://web.test/api/chats/web-chat-1/stream?assistantMessageId=assistant-1&runId=run-1",
      { method: "GET" },
    );
    Object.defineProperty(request, "nextUrl", {
      value: new URL("http://web.test/api/chats/web-chat-1/stream?assistantMessageId=assistant-1&runId=run-1"),
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

  it("ignores a malformed event cursor but requires a terminal receipt", async () => {
    const { GET } = await import("@/app/api/chats/[chatId]/stream/route");
    const backend = makeBackendFrameStream([
      'data: {"type":"session_info","session_id":"runtime-session-1","run_id":"run-1"}\n\n',
      'data: {"type":"run_input_queued","run_id":"run-1","index":"4"}\n\n',
      'data: {"type":"text_done","full_text":"should not win"}\n\n',
    ]);
    setAdmittedRuntime({
      sdk: {
        listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: vi.fn().mockResolvedValue({
        ok: true,
        headers: backend.headers,
        body: backend.body,
      }),
    } as never);
    const request = new Request(
      "http://web.test/api/chats/web-chat-1/stream?assistantMessageId=assistant-1&runId=run-1",
      { method: "GET" },
    );
    Object.defineProperty(request, "nextUrl", {
      value: new URL("http://web.test/api/chats/web-chat-1/stream?assistantMessageId=assistant-1&runId=run-1"),
    });

    const response = await GET(request as never, {
      params: Promise.resolve({ chatId: "web-chat-1" }),
    });

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    let body = "";
    for (;;) {
      const { done, value } = await reader!.read();
      if (done) {
        break;
      }
      body += new TextDecoder().decode(value);
    }

    expect(body).not.toContain("Malformed stream event index.");
    expect(body).toContain("should not win");
    expect(backend.cancel).not.toHaveBeenCalled();
    expect(mockSetChatActiveRun).not.toHaveBeenCalledWith("user-a", "web-chat-1", undefined);
    expect(body).toContain("stream_observation_interrupted");
    expect(mockUpdateStreamingAssistantMessage).toHaveBeenLastCalledWith(
      "user-a",
      "web-chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "should not win",
        status: "streaming",
      }),
    );
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalledWith(
      "user-a",
      "web-chat-1",
      "assistant-1",
      expect.objectContaining({
        content: "Malformed stream event index.",
        status: "failed",
      }),
    );
  });
});

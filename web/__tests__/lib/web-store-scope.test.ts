vi.mock("@/lib/runtime-config", () => ({
  getRuntimeConfig: vi.fn(),
}));

import {
  createChatWithMessage,
  getChatHydrated,
  getStore,
  listChats,
  setChatActiveRun,
} from "@/lib/api/web-store";
import { getRuntimeConfig } from "@/lib/runtime-config";

const mockGetRuntimeConfig = vi.mocked(getRuntimeConfig);

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status >= 200 && status < 300 ? "OK" : "Error",
    json: vi.fn().mockResolvedValue(body),
    text: vi.fn().mockResolvedValue(JSON.stringify(body)),
    headers: new Headers({ "content-type": "application/json" }),
  };
}

function runtimeSession(sessionId: string, userId: string, title: string) {
  return {
    session_id: sessionId,
    user_id: userId,
    title,
    metadata: {
      source: "web_v1",
    },
    status: "active",
    created_at: new Date().toISOString(),
    updated_at: new Date().toISOString(),
  };
}

function runtimeSessionList(sessions: unknown[]) {
  return {
    sessions,
    total: sessions.length,
    limit: 200,
    next_cursor: null,
  };
}

function runtimeRun(
  runId: string,
  sessionId: string,
  status: string,
  waitingFor?: string | null,
) {
  return {
    run_id: runId,
    session_id: sessionId,
    status,
    waiting_for: waitingFor ?? null,
    events_count: 0,
  };
}

function runtimeRunList(runs: unknown[]) {
  return {
    runs,
    total: runs.length,
    limit: 200,
  };
}

function runtimeTranscript(items: unknown[] = []) {
  return {
    items,
    total: items.length,
    limit: 200,
    offset: 0,
  };
}

describe("web store user scoping", () => {
  beforeEach(() => {
    globalThis.__astraWebStores = undefined;
    mockGetRuntimeConfig.mockResolvedValue({
      mode: "live",
      source: "cookie",
      apiUrl: "http://runtime.test",
      accessToken: "test-token",
      refreshToken: undefined,
      demoMode: false,
      hasAccessToken: true,
      hasRefreshToken: false,
      maskedAccessToken: "test-token",
      message: "test runtime",
    });
    globalThis.fetch = vi.fn().mockResolvedValue(
      jsonResponse(
        {
          session_id: "session-user-a",
          user_id: "user-a",
          title: "test",
          metadata: {},
          status: "active",
          created_at: new Date().toISOString(),
          updated_at: null,
        },
        201,
      ),
    );
  });

  it("does not expose one user chat list to another authenticated user", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    const uniqueMessage = `private scoped prompt ${crypto.randomUUID()}`;
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("session-user-a", "user-a", "test"),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeRunList([])))
      .mockResolvedValueOnce(jsonResponse(runtimeSessionList([])))
      .mockResolvedValueOnce(jsonResponse(runtimeRunList([])));

    const result = await createChatWithMessage("user-a", {
      message: uniqueMessage,
      model: "sonnet-4.6-adaptive",
      options: {
        webSearch: false,
        thinking: true,
        activeSkills: ["skill-creator"],
      },
      projectId: null,
    });

    expect(result.chatId).toMatch(/^web-/);
    expect(
      (await listChats("user-a", { q: uniqueMessage })).items,
    ).toHaveLength(1);
    expect(
      (await listChats("user-b", { q: uniqueMessage })).items,
    ).toHaveLength(0);
  });

  it("preserves pending local chat shells before their first stream creates a persisted session", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(jsonResponse(runtimeSessionList([])))
      .mockResolvedValueOnce(jsonResponse(runtimeRunList([])))
      .mockResolvedValueOnce(jsonResponse(runtimeSessionList([])))
      .mockResolvedValueOnce(jsonResponse(runtimeRunList([])))
      .mockResolvedValueOnce(jsonResponse(runtimeSessionList([])))
      .mockResolvedValueOnce(jsonResponse(runtimeRunList([])));

    const result = await createChatWithMessage("user-a", {
      message: "this chat will disappear remotely",
      model: "sonnet-4.6-adaptive",
      options: {
        webSearch: false,
        thinking: true,
        activeSkills: [],
      },
      projectId: null,
    });

    expect(result.chatId).toMatch(/^web-/);
    expect(
      (await listChats("user-a", { q: "disappear remotely" })).items,
    ).toHaveLength(1);

    await expect(getChatHydrated("user-a", result.chatId)).resolves.toEqual(
      expect.objectContaining({
        pendingTurn: expect.objectContaining({
          content: "this chat will disappear remotely",
        }),
      }),
    );
    expect(
      (await listChats("user-a", { q: "disappear remotely" })).items,
    ).toHaveLength(1);
  });

  it("stores an initial workspace selection on the pending local chat shell", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(jsonResponse(runtimeSessionList([])))
      .mockResolvedValueOnce(jsonResponse(runtimeRunList([])));

    const result = await createChatWithMessage("user-a", {
      message: "list files in this workspace",
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
    });

    await expect(getChatHydrated("user-a", result.chatId)).resolves.toEqual(
      expect.objectContaining({
        workspaceSelection: {
          kind: "edge_workspace",
          edgeAgentId: "edge-1",
          displayName: "MacBook Pro",
          cwd: "/Users/test/astra",
        },
        workspaceSelectionExplicit: true,
        pendingTurn: expect.objectContaining({
          content: "list files in this workspace",
        }),
      }),
    );
  });

  it("syncs a persisted backend session back onto the existing local web chat id", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    const result = await createChatWithMessage("user-a", {
      message: "bind this local shell",
      model: "sonnet-4.6-adaptive",
      options: {
        webSearch: false,
        thinking: true,
        activeSkills: [],
      },
      projectId: null,
    });
    const store = getStore("user-a");
    const localChat = store.chats.find((chat) => chat.id === result.chatId);
    expect(localChat).toBeDefined();
    localChat!.backendSessionId = "runtime-session-1";
    localChat!.pendingTurn = undefined;

    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("runtime-session-1", "user-a", "bound"),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeRunList([])));

    const items = (await listChats("user-a", { q: "bind this local shell" }))
      .items;
    expect(items).toHaveLength(1);
    expect(items[0]?.id).toBe(result.chatId);
    expect(
      store.chats.filter(
        (chat) => chat.backendSessionId === "runtime-session-1",
      ),
    ).toHaveLength(1);
  });

  it("deduplicates backend-session chat records in favor of the web chat id", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    const store = getStore("user-a");
    store.chats.push(
      {
        id: "web-chat-1",
        title: "web copy",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        lastMessageAt: "2026-06-07T00:00:00.000Z",
        lastMessagePreview: "hello from web",
        model: "sonnet-4.6-adaptive",
        backendSessionId: "runtime-session-1",
        messages: [],
        activeRun: undefined,
      },
      {
        id: "runtime-session-1",
        title: "backend duplicate",
        projectId: null,
        createdAt: "2026-06-07T00:00:00.000Z",
        lastMessageAt: "2026-06-07T00:00:00.000Z",
        lastMessagePreview: "hello from backend duplicate",
        model: "sonnet-4.6-adaptive",
        backendSessionId: "runtime-session-1",
        messages: [],
        activeRun: undefined,
      },
    );
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("runtime-session-1", "user-a", "web copy"),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeRunList([])));

    const items = (await listChats("user-a", { q: "web copy" })).items;

    expect(items).toHaveLength(1);
    expect(items[0]?.id).toBe("web-chat-1");
    expect(store.chats.map((chat) => chat.id)).not.toContain(
      "runtime-session-1",
    );
    expect(
      store.chats.filter(
        (chat) => chat.backendSessionId === "runtime-session-1",
      ),
    ).toHaveLength(1);
  });

  it("prefers the most interactive active run when one chat has multiple non-terminal runs", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("session-multi-run", "user-a", "multi"),
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeRunList([
            runtimeRun("run-paused", "session-multi-run", "paused"),
            runtimeRun("run-running", "session-multi-run", "running"),
            runtimeRun(
              "run-waiting",
              "session-multi-run",
              "waiting",
              "user_input",
            ),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeTranscript([])));

    const detail = await getChatHydrated("user-a", "session-multi-run");
    expect(detail?.activeRun).toEqual({
      runId: "run-waiting",
      status: "waiting",
      waitingFor: "user_input",
      nextEventIndex: 0,
    });
  });

  it("keeps the freshest backend run when competing active runs have the same priority", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("session-running-race", "user-a", "race"),
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeRunList([
            runtimeRun("run-freshest", "session-running-race", "running"),
            runtimeRun("run-stale", "session-running-race", "running"),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeTranscript([])));

    const detail = await getChatHydrated("user-a", "session-running-race");
    expect(detail?.activeRun).toEqual({
      runId: "run-freshest",
      status: "running",
      waitingFor: null,
      nextEventIndex: 0,
    });
  });

  it("preserves fresher local run state when backend polling lags behind the stream", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("session-stream-lag", "user-a", "lag"),
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeRunList([
            runtimeRun("run-stream-lag", "session-stream-lag", "running"),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeTranscript([])));

    getStore("user-a").chats.push({
      id: "session-stream-lag",
      title: "lag",
      projectId: null,
      createdAt: "2026-06-07T00:00:00.000Z",
      lastMessageAt: "2026-06-07T00:00:00.000Z",
      lastMessagePreview: "hello",
      model: "sonnet-4.6-adaptive",
      messages: [],
      activeRun: undefined,
    });
    setChatActiveRun("user-a", "session-stream-lag", {
      runId: "run-stream-lag",
      status: "input-queued",
      waitingFor: "user_input",
    });

    const detail = await getChatHydrated("user-a", "session-stream-lag");
    expect(detail?.activeRun).toEqual({
      runId: "run-stream-lag",
      status: "input-queued",
      waitingFor: "user_input",
    });
  });

  it("clears ghost local active runs once the backend reports the same run as terminal", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("session-terminal-sync", "user-a", "terminal"),
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeRunList([
            runtimeRun(
              "run-terminal-sync",
              "session-terminal-sync",
              "completed",
            ),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeTranscript([])));

    getStore("user-a").chats.push({
      id: "session-terminal-sync",
      title: "terminal",
      projectId: null,
      createdAt: "2026-06-07T00:00:00.000Z",
      lastMessageAt: "2026-06-07T00:00:00.000Z",
      lastMessagePreview: "hello",
      model: "sonnet-4.6-adaptive",
      messages: [],
      activeRun: undefined,
    });
    setChatActiveRun("user-a", "session-terminal-sync", {
      runId: "run-terminal-sync",
      status: "running",
      waitingFor: null,
    });

    const detail = await getChatHydrated("user-a", "session-terminal-sync");
    expect(detail?.activeRun).toBeUndefined();
  });
});

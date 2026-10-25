vi.mock("@/lib/runtime-config", () => ({
  getRuntimeConfig: vi.fn(),
}));

import {
  createChatWithMessage,
  getChatHydrated,
  getChat,
  getStore,
  listChats,
  setChatActiveRun,
  updateStreamingAssistantMessage,
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
  lineage?: {
    parentRunId: string | null;
    rootRunId: string;
    depth: number;
  },
) {
  return {
    run_id: runId,
    session_id: sessionId,
    parent_run_id: lineage?.parentRunId ?? null,
    root_run_id: lineage?.rootRunId ?? runId,
    depth: lineage?.depth ?? 0,
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

  it("keeps transcript and chat ordering unchanged when replay repairs graph facts", () => {
    const timestamp = "2026-06-07T00:00:00.000Z";
    const message = { id: "assistant-observation", role: "assistant" as const,
      content: "live answer", createdAt: timestamp, status: "streaming" as const };
    const chat = { id: "chat-observation", title: "Observation", projectId: null,
      createdAt: timestamp, lastMessageAt: timestamp, lastMessagePreview: "live answer",
      model: "sonnet-4.6-adaptive", messages: [message] };
    getStore("user-a").chats.push(chat);
    const fact = { type: "explain_analyze" as const, schema_version: 1 as const,
      event_id: "clock:1", run_id: "run-1", turn_id: "turn-1", node_id: "turn-1",
      producer_id: "worker", clock_domain_id: "clock", kind: "turn" as const,
      label: "Answer", transition: "started" as const, elapsed_ms: 0 };
    updateStreamingAssistantMessage("user-a", chat.id, message.id, { explainAnalyzeDegraded: true });
    updateStreamingAssistantMessage("user-a", chat.id, message.id,
      { explainAnalyzeRepair: { token: "full-replay", complete: false } });
    updateStreamingAssistantMessage("user-a", chat.id, message.id, { explainAnalyzeEvent: fact });
    updateStreamingAssistantMessage("user-a", chat.id, message.id, { explainAnalyzeEvent: fact });
    updateStreamingAssistantMessage("user-a", chat.id, message.id,
      { explainAnalyzeRepair: { token: "full-replay", complete: true } });
    const saved = getStore("user-a").chats.find((item) => item.id === chat.id)!;
    expect(saved.lastMessageAt).toBe(timestamp);
    expect(saved.lastMessagePreview).toBe("live answer");
    expect(saved.messages[0]).toMatchObject({ content: "live answer", status: "streaming",
      explainAnalyzeEvents: [fact], explainAnalyzeDegraded: false });
  });

  it("preserves persisted artifacts when a resumed stream adds another artifact", () => {
    getStore("user-a").chats.push({
      id: "chat-artifacts",
      title: "Artifacts",
      projectId: null,
      createdAt: "2026-06-07T00:00:00.000Z",
      lastMessageAt: "2026-06-07T00:00:00.000Z",
      lastMessagePreview: "",
      model: "sonnet-4.6-adaptive",
      messages: [
        {
          id: "assistant-artifacts",
          role: "assistant",
          content: "",
          createdAt: "2026-06-07T00:00:00.000Z",
          status: "streaming",
          artifacts: [
            { id: "artifact-a", kind: "file", filename: "a.pdf" },
          ],
        },
      ],
    });

    updateStreamingAssistantMessage(
      "user-a",
      "chat-artifacts",
      "assistant-artifacts",
      {
        artifacts: [
          { id: "artifact-b", kind: "file", filename: "b.pdf" },
        ],
      },
    );

    expect(getStore("user-a").chats[0]?.messages[0]?.artifacts).toEqual([
      expect.objectContaining({ id: "artifact-a" }),
      expect.objectContaining({ id: "artifact-b" }),
    ]);
  });

  it("hydrates artifacts from the durable transcript after Web process state is lost", async () => {
    const timestamp = "2026-09-01T00:00:00.000Z";
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("session-artifact-history", "user-a", "Artifacts"),
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          { kind: "conversation", session: runtimeSession("session-artifact-history", "user-a", "Artifacts"), selected_root: null, items: [
            {
              kind: "raw", session_id: "session-artifact-history",
              item_seq: 1,
              run_id: "run-artifact-history",
              role: "assistant",
              content: "Created the report.",
              artifacts: [
                {
                  artifact_id: "artifact-1",
                  type: "file",
                  name: "report.pdf",
                  data: { download_url: "/api/files/file-1" },
                },
              ],
              created_at: timestamp,
            },
          ], history_loaded: true, raw_page: { has_more: false, next_before_seq: 1 }, truncated: false },
        ),
      );

    const detail = await getChatHydrated(
      "user-a",
      "session-artifact-history",
    );

    expect(detail?.messages[0]?.artifacts).toEqual([
      expect.objectContaining({
        id: "artifact-1",
        filename: "report.pdf",
        downloadUrl: "/api/files/file-1",
      }),
    ]);
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
      );

    await listChats("user-a", {});
    const detail = getChat("user-a", "session-multi-run");
    expect(detail?.activeRun).toMatchObject({
      runId: "run-waiting",
      status: "waiting",
      waitingFor: "user_input",
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
      );

    await listChats("user-a", {});
    const detail = getChat("user-a", "session-running-race");
    expect(detail?.activeRun).toMatchObject({
      runId: "run-freshest",
      status: "running",
      waitingFor: null,
    });
  });

  it("keeps the root conversation run authoritative when a child agent is newer and active", async () => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            runtimeSession("session-fanout", "user-a", "fanout"),
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeRunList([
            runtimeRun("child-newer", "session-fanout", "waiting", "tool", {
              parentRunId: "root-run",
              rootRunId: "root-run",
              depth: 1,
            }),
            runtimeRun("root-run", "session-fanout", "running"),
          ]),
        ),
      );

    await listChats("user-a", {});
    const detail = getChat("user-a", "session-fanout");

    expect(detail?.activeRun).toMatchObject({
      runId: "root-run",
      status: "running",
      waitingFor: null,
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
      );

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

    await listChats("user-a", {});
    const detail = getChat("user-a", "session-stream-lag");
    expect(detail?.activeRun).toMatchObject({
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
      );

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

    await listChats("user-a", {});
    const detail = getChat("user-a", "session-terminal-sync");
    expect(detail?.activeRun).toBeUndefined();
  });

  it("resolves a legacy Web id once, then reads the known Session directly", async () => {
    const persisted = { ...runtimeSession("session-restart", "user-a", "Restart"),
      metadata: { source: "web_v1", web_chat_id: "web-stable" } };
    const projection = { kind: "conversation", session: persisted,
      selected_root: { run_id: "root", status: "running", waiting_for: null },
      items: [{ kind: "opening", id: "input:root", run_id: "root", source_event_id: "source", content: " 你好\n ", created_at: "2026-10-25T03:00:00.123456Z" }],
      history_loaded: true, raw_page: { has_more: false, next_before_seq: null }, truncated: false };
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock.mockResolvedValueOnce(jsonResponse(runtimeSessionList([persisted])))
      .mockResolvedValueOnce(jsonResponse(projection)).mockResolvedValueOnce(jsonResponse(projection));
    const first = await getChatHydrated("user-a", "web-stable");
    const second = await getChatHydrated("user-a", "web-stable");
    expect(second?.chat.id).toBe("web-stable");
    expect(second?.messages).toBe(first?.messages);
    expect(second?.messages[0].content).toBe(" 你好\n ");
    expect(second?.activeRun?.nextEventIndex).toBe(0);
    expect(fetchMock.mock.calls.map(call => new URL(call[0]).pathname)).toEqual([
      "/sessions", "/sessions/session-restart/transcript", "/sessions/session-restart/transcript",
    ]);
  });
});

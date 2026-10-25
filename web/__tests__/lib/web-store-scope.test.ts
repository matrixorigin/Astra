vi.mock("@/lib/runtime-config", () => ({
  getRuntimeConfig: vi.fn(),
}));

import {
  createChatWithMessage,
  getChatHydrated,
  getStore,
  listChats,
  sendMessage,
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
  input?: { original_input: string; source_event_id: string; created_at?: string },
) {
  return {
    ...(input ? { opening_input: { created_at: "2026-10-25T03:00:00Z", ...input } } : {}),
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
          runtimeRunList([
            runtimeRun(
              "run-artifact-history",
              "session-artifact-history",
              "completed",
            ),
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeTranscript([
            {
              session_id: "session-artifact-history",
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
          ]),
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

  it("rejects an invalid model selection without mutating the chat or creating a backend session", async () => {
    const store = getStore("user-a");
    const chat = {
      id: "web-invalid-model",
      title: null,
      projectId: null,
      createdAt: "2026-07-21T00:00:00.000Z",
      lastMessageAt: "2026-07-21T00:00:00.000Z",
      lastMessagePreview: "existing message",
      model: "offering-a",
      messages: [],
      activeRun: undefined,
    };
    store.chats.push(chat);
    const before = JSON.parse(JSON.stringify(chat));
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;

    await expect(
      sendMessage("user-a", chat.id, {
        content: "must not be appended",
        options: {
          model: "   ",
          webSearch: false,
          thinking: false,
        },
      }),
    ).rejects.toMatchObject({ code: "invalid_selection" });

    expect(chat).toEqual(before);
    expect(fetchMock).not.toHaveBeenCalled();
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
              "user_input", undefined,
              { original_input: "hello", source_event_id: "source-run-waiting" },
            ),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeTranscript([])))
      .mockResolvedValueOnce(jsonResponse({
        run_id: "run-waiting", session_id: "session-multi-run", status: "running",
        events_count: 0,
      }));

    const detail = await getChatHydrated("user-a", "session-multi-run");
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
            runtimeRun("run-freshest", "session-running-race", "running", null, undefined, { original_input: "hello", source_event_id: "source-run-freshest" }),
            runtimeRun("run-stale", "session-running-race", "running"),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeTranscript([])))
      .mockResolvedValueOnce(jsonResponse({
        run_id: "run-freshest", session_id: "session-running-race", status: "running",
        events_count: 0,
      }));

    const detail = await getChatHydrated("user-a", "session-running-race");
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
            runtimeRun("root-run", "session-fanout", "running", null, undefined, { original_input: "hello", source_event_id: "source-root-run" }),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeTranscript([])))
      .mockResolvedValueOnce(jsonResponse({
        run_id: "root-run", session_id: "session-fanout", status: "running",
        events_count: 0,
      }));

    const detail = await getChatHydrated("user-a", "session-fanout");

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
            runtimeRun("run-stream-lag", "session-stream-lag", "running", null, undefined, { original_input: "hello", source_event_id: "source-run-stream-lag" }),
          ]),
        ),
      )
      .mockResolvedValueOnce(jsonResponse(runtimeTranscript([])))
      .mockResolvedValueOnce(jsonResponse({
        run_id: "run-stream-lag", session_id: "session-stream-lag", status: "running",
        events_count: 0,
      }));

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

  it.each([
    { name: "empty transcript", history: [], input: "run agents" },
    { name: "prior identical text", history: [{ item_seq: 1, run_id: "old-run", role: "user", content: "run agents" }], input: "run agents" },
    { name: "exact empty input", history: [], input: "" },
    { name: "Unicode and whitespace", history: [], input: " 你好\n世界 \n" },
    { name: "already committed current input", history: [{ item_seq: 1, run_id: "root-restart", source_event_id: "input-source", role: "user", content: "run agents" }], input: "run agents" },
  ])("recovers opening input and assistant after state loss: $name", async ({ history, input }) => {
    const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeSessionList([
            {
              ...runtimeSession("session-restart", "user-a", "Restart"),
              metadata: { source: "web_v1", web_chat_id: "web-stable" },
            },
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeRunList([
            runtimeRun("root-restart", "session-restart", "running", null, undefined,
              { original_input: input, source_event_id: "input-source" }),
          ]),
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse(
          runtimeTranscript(history),
        ),
      )
      .mockResolvedValueOnce(jsonResponse({
        run_id: "root-restart", session_id: "session-restart", status: "running",
        events_count: 0,
      }));

    const detail = await getChatHydrated("user-a", "web-stable");
    expect(detail?.messages.filter(message => message.role === "user")).toHaveLength(
      history.some(item => item.run_id === "root-restart") ? 1 : history.length + 1,
    );
    expect(detail?.messages.at(-2)?.content).toBe(input);
    expect(fetchMock.mock.calls[1]?.[0]).toContain("include_input_for_session=session-restart");
    expect(fetchMock.mock.calls.at(-1)?.[0]).toContain("/chat/runs/root-restart");

    expect(detail?.chat.id).toBe("web-stable");
    expect(detail?.session?.backendSessionId).toBe("session-restart");
    expect(detail?.messages.at(-1)).toMatchObject({
      id: "inflight:root-restart",
      role: "assistant",
      status: "streaming",
    });
    expect(detail?.activeRun?.assistantMessageId).toBe("inflight:root-restart");
    expect(detail?.activeRun?.nextEventIndex).toBe(0);
  });
  it.each(["terminal", "missing input", "wrong session", "read error", "replaced run", "conflicting input"])(
    "handles cold recovery %s without publishing stale overlays", async scenario => {
      const fetchMock = globalThis.fetch as ReturnType<typeof vi.fn>;
      fetchMock
        .mockResolvedValueOnce(jsonResponse(runtimeSessionList([runtimeSession("session-cold", "user-a", "Cold")])))
        .mockResolvedValueOnce(jsonResponse(runtimeRunList([runtimeRun("run-cold", "session-cold", "running", null, undefined,
          scenario === "missing input" ? undefined : { original_input: "original input", source_event_id: "input-source" })])))
        .mockResolvedValueOnce(jsonResponse(runtimeTranscript(scenario === "conflicting input" ? [
          { item_seq: 1, run_id: "run-cold", source_event_id: "input-source", role: "user", content: "altered input" },
        ] : [])))
        .mockImplementationOnce(async () => {
          if (scenario === "replaced run") {
            const chat = getStore("user-a").chats[0];
            chat.messages = [{ id: "new-user", role: "user", content: "new turn", createdAt: new Date().toISOString() }];
            setChatActiveRun("user-a", chat.id, { runId: "new-run", status: "running" });
          }
          if (scenario === "read error") return jsonResponse({ error: "storage unavailable" }, 503);
          return jsonResponse({ run_id: "run-cold", session_id: scenario === "wrong session" ? "other" : "session-cold",
            status: scenario === "terminal" ? "completed" : "running",
            events_count: 0,
          });
        });
      if (scenario === "terminal") {
        fetchMock.mockResolvedValueOnce(jsonResponse(runtimeTranscript([
          { item_seq: 1, run_id: "run-cold", source_event_id: "input-source", role: "user", content: "original input" },
          { item_seq: 2, run_id: "run-cold", role: "assistant", content: "done" },
        ])));
        const detail = await getChatHydrated("user-a", "session-cold");
        expect(detail?.activeRun).toBeUndefined();
        expect(detail?.messages.map(message => message.content)).toEqual(["original input", "done"]);
        expect(detail?.messages.every(message => message.status === "complete")).toBe(true);
      } else if (scenario === "replaced run") {
        const detail = await getChatHydrated("user-a", "session-cold");
        expect(detail?.activeRun?.runId).toBe("new-run");
        expect(detail?.messages.map(message => message.content)).toEqual(["new turn"]);
      } else {
        await expect(getChatHydrated("user-a", "session-cold")).rejects.toThrow(/recover (?:input|run)/);
        expect(getStore("user-a").chats[0].messages).toEqual([]);
      }
    },
  );

});

// @vitest-environment node
vi.mock("@/lib/api/auth-guard", () => ({ requireRuntimeUser: vi.fn() }));
vi.mock("@/lib/runtime-client/server", () => ({
  WebRuntimeClient: class { constructor(public config: unknown) {} },
  getRuntimeClient: vi.fn(), requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { WebRuntimeClient, getRuntimeClient, requireRuntimeClient } from "@/lib/runtime-client/server";
import { createProject, ensureChatBackendSession, updateChatModel, updateChatWorkspaceSelection, archiveChat, moveChat, deleteChat, getChat, getChatHydrated, getStore, listChats, queueDeferredRunInput, stopActiveRun, updateStreamingAssistantMessage } from "@/lib/api/web-store";
import { GET as GETDetail } from "@/app/api/chats/[chatId]/route";
import { DELETE as DELETEArchived } from "@/app/api/chats/route";
import { POST as POSTMessage } from "@/app/api/chats/[chatId]/messages/route";
import { GET, POST as POSTStream } from "@/app/api/chats/[chatId]/stream/route";
import { AstraApiError, type RuntimeConversationResponse } from "@astra/sdk";

const session = { session_id: "session", created_at: "2026-10-25T03:00:00Z", status: "active", metadata: { source: "web_v1" } };
function conversation(runId = "run", status = "running", content = "original request"): RuntimeConversationResponse {
  const snapshot: RuntimeConversationResponse = { kind: "conversation", session,
    selected_root: { run_id: runId, status, waiting_for: null, created_at: "2026-10-25T03:00:00.123456Z" },
    items: [{ kind: "opening", id: `input:${runId}`, run_id: runId, source_event_id: `source:${runId}`,
      created_at: "2026-10-25T03:00:00.123456Z", content }],
    history_loaded: true, raw_page: { next_before_seq: null, has_more: false }, truncated: false };
  if (status === "failed") snapshot.items.push({kind: "failure", id: `failure:${runId}`, run_id:runId,
    created_at:"2026-10-25T03:00:00.123456Z", failure:{message:"Astra run failed.", error_code:null}});
  return snapshot;
}
function setup() {
  globalThis.__astraWebStores = undefined;
  const sdk = {
    getTokens: vi.fn(() => ({ accessToken: "synthetic-test", refreshToken: null })),
    listRuntimeSessions: vi.fn().mockResolvedValue({ sessions: [session] }),
    listRuns: vi.fn().mockResolvedValue({ runs: [{ runId: "run", sessionId: "session", parentRunId: null, depth: 0, status: "running" }] }),
    getSessionConversation: vi.fn().mockResolvedValue(conversation()),
    getRunStatus: vi.fn().mockResolvedValue({ runId: "run", sessionId: "session", status: "running", eventsCount: 4 }),
    createRun: vi.fn().mockResolvedValue({ runId: "run", sessionId: "session", status: "running" }),
    createRuntimeSession: vi.fn().mockResolvedValue(session),
    deleteSession: vi.fn().mockResolvedValue(undefined),
    cancelRun: vi.fn(() => new Promise(() => {})),
    submitRunInput: vi.fn().mockResolvedValue({ accepted: true }),
    listSessionArtifacts: vi.fn().mockResolvedValue({ artifacts: [] }),
  };
  const fetchResponse = vi.fn().mockImplementation(async () => new Response([
    { type: "run_started", run_id: "run", index: 0 },
    { type: "text_delta", content: "recovered answer", index: 1 },
    { type: "run_finished", run_id: "run", status: "completed", index: 2 },
  ].map(event => `data: ${JSON.stringify(event)}\n\n`).join(""), {
    headers: { "x-astra-agent-interaction-api-major": "3" },
  }));
  const runtime = Object.assign(new WebRuntimeClient({ accessToken: "synthetic-test" } as never), { sdk, fetchResponse });
  vi.mocked(requireRuntimeUser).mockResolvedValue({ user: { user_id: "owner", display_name: null }, client: runtime, response: null } as never);
  vi.mocked(getRuntimeClient).mockResolvedValue(runtime as never);
  vi.mocked(requireRuntimeClient).mockResolvedValue(runtime as never);
  return runtime;
}
it.each(["warm", "cold", "successor"])("preserves failed partial answers across %s detail refreshes", async mode => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  runtime.fetchResponse.mockResolvedValue(new Response([
    { type: "run_started", run_id: "run", index: 0 },
    { type: "text_delta", content: "Only the first half", index: 1 },
    { type: "run_error", run_id: "run", error: "Provider connection lost", error_code: "stream_transport", index: 2 },
    { type: "run_finished", run_id: "run", status: "failed", owner_generation: 0, index: 3 },
  ].map(event => `data: ${JSON.stringify(event)}\n\n`).join(""), {
    headers: { "x-astra-agent-interaction-api-major": "3" },
  }));
  const live = await stream();
  expect(live.status).toBe(200);
  await live.text();
  expect(getChat("owner", "session")?.messages.at(-1)).toMatchObject({ status: "failed" });
  const persisted = conversation("run", "failed");
  persisted.items = persisted.items.filter(item => item.kind !== "failure");
  persisted.items.push({ kind: "raw", session_id: "session", item_seq: 2, run_id: "run", role: "assistant", content: "Only the first half", created_at:"2026-10-25T03:01:00Z", failure: { message: "Provider connection lost", error_code: "stream_transport" } });
  if (mode === "cold") globalThis.__astraWebStores = undefined;
  if (mode === "successor") {
    const next = conversation("next", "completed", "next request");
    next.items.unshift(...persisted.items);
    next.items.push({ kind: "raw", session_id: "session", item_seq: 3, run_id: "next", role: "assistant", content: "new complete answer", created_at:"2026-10-25T03:02:00Z" });
    runtime.sdk.getSessionConversation.mockResolvedValue(next);
  } else runtime.sdk.getSessionConversation.mockResolvedValue(persisted);
  const detail = await GETDetail({} as never, { params: Promise.resolve({ chatId: "session" }) });
  expect(detail.status).toBe(200);
  const body = await detail.json();
  expect(body.messages.find((message: {content: string}) => message.content.startsWith("Only the first half"))).toMatchObject({ content: "Only the first half\n\nProvider connection lost", status: "failed" });
  const again = await GETDetail({} as never, { params: Promise.resolve({chatId: "session"}) });
  expect((await again.json()).messages).toEqual(body.messages);
  if (mode === "successor") expect(body.messages.at(-1)).toMatchObject({ content: "new complete answer", status: "complete" });
});

async function stream() {
  return GET({ nextUrl: new URL("http://web.test/api/chats/session/stream?runId=run&last_index=0") } as never,
    { params: Promise.resolve({ chatId: "session" }) });
}

it("hydrates an opening-only Run and actually resumes its stream to completion", async () => {
  const runtime = setup();
  expect((await getChatHydrated("owner", "session"))?.activeRun?.nextEventIndex).toBe(0);
  const response = await stream();
  expect(response.status).toBe(200);
  expect(await response.text()).toContain("recovered answer");
  expect(runtime.fetchResponse).toHaveBeenCalledWith("/chat/runs/run/stream?last_index=0", expect.objectContaining({ method: "GET" }));
  const completed = getChat("owner", "session");
  expect(completed?.messages.map(message => message.content)).toEqual(["original request", "recovered answer"]);
  expect(completed?.messages[1].status).toBe("complete");
  expect(completed?.activeRun).toBeUndefined();
});

it.each(["completed", "failed"])("retains the exact warm parser and deferred guidance when database %s arrives first", async terminalStatus => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  updateStreamingAssistantMessage("owner", "session", first!.activeRun!.assistantMessageId!, { content: "first partial" });
  const next = await queueDeferredRunInput("owner", "session", { content: "guidance", pendingMessageId: "guidance-1" });
  updateStreamingAssistantMessage("owner", "session", next!.assistantMessage.id, { content: "second partial" });
  const before = getChat("owner", "session")!.messages;
  const parser = getStore("owner").chats[0].streamProjection;
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("run", terminalStatus));
  const hydrated = await getChatHydrated("owner", "session");
  expect(hydrated?.messages).toBe(before);
  expect(getStore("owner").chats[0].streamProjection).toBe(parser);
  expect(hydrated?.messages.map(message => message.content)).toEqual(["original request", "first partial", "guidance", "second partial"]);
  expect(hydrated?.activeRun?.assistantMessageId).toBe(next!.assistantMessage.id);
  expect(hydrated?.activeRun?.nextEventIndex).toBe(4);
  expect(runtime.sdk.getSessionConversation).toHaveBeenCalledTimes(2);
  expect(runtime.sdk.listRuntimeSessions).toHaveBeenCalledTimes(2);
  expect(runtime.sdk.listRuns).toHaveBeenCalledTimes(1);
});

it.each(["failed", "cancelled"])("keeps the exact accepted input on a cold %s Run without a live placeholder", async status => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("run", status, " \n你好 "));
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages.map(message => message.content)).toEqual(status === "failed" ? [" \n你好 ", "Astra run failed."] : [" \n你好 "]);
  expect(detail?.activeRun).toBeUndefined();
  expect(runtime.sdk.getRunStatus).not.toHaveBeenCalled();
});

it("discovers an externally completed successor from a nonempty opening-only idle view", async () => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("run", "failed"));
  await getChatHydrated("owner", "session");
  const next = conversation("next", "completed", "next request");
  next.items.unshift(...conversation("run", "failed").items);
  next.items.push({ kind: "raw", session_id: "session", item_seq: 2, run_id: "next", role: "assistant", content: "new answer" });
  runtime.sdk.getSessionConversation.mockResolvedValue(next);
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "Astra run failed.", "next request", "new answer"]);
  expect(detail?.activeRun).toBeUndefined();
});

it("reads one bounded projection on cold and warm details without enumerating Run inputs", async () => {
  const runtime = setup();
  const bounded = conversation();
  bounded.items = Array.from({ length: 200 }, (_, i) => ({ ...bounded.items[0], id: `input:${i}`, content: `request ${i}` }));
  runtime.sdk.getSessionConversation.mockResolvedValue(bounded);
  const first = await getChatHydrated("owner", "session");
  const second = await getChatHydrated("owner", "session");
  expect(first?.messages).toHaveLength(201);
  expect(second?.messages).toBe(first?.messages);
  expect(runtime.sdk.getSessionConversation.mock.calls).toEqual([["session", 200, undefined], ["session", 200, { runId: "run", preserveTerminal: true }]]);
  expect(runtime.sdk.listRuns).not.toHaveBeenCalled();
});

it.each([false, true])("fences a stale conversation response/error when deferred input changes the binding (error=%s)", async fail => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  runtime.sdk.getSessionConversation.mockImplementationOnce(async () => {
    await queueDeferredRunInput("owner", "session", { content: "guidance", pendingMessageId: "guidance-1" });
    if (fail) throw new Error("stale read error");
    return conversation("run", "failed");
  });
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "", "guidance", ""]);
  expect(detail?.activeRun?.nextEventIndex).toBe(4);
});

it("discards a stale recovery after another reader replaces the same assistant's parser", async () => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  getStore("owner").chats[0].streamProjection!.state.protocolError = true;
  runtime.sdk.getSessionConversation.mockImplementationOnce(async () => {
    await getChatHydrated("owner", "session");
    updateStreamingAssistantMessage("owner", "session", first!.activeRun!.assistantMessageId!, { content: "newer recovery" });
    return conversation("run", "failed");
  });
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "newer recovery"]);
  expect(detail?.activeRun?.assistantMessageId).toBe(first!.activeRun!.assistantMessageId);
});

it("retires the old stream writer before publishing an active or terminal successor", async () => {
  for (const status of ["running", "completed"]) {
    const runtime = setup();
    await getChatHydrated("owner", "session");
    let oldTerminal!: ReadableStreamDefaultController<Uint8Array>;
    runtime.fetchResponse.mockResolvedValueOnce(new Response(new ReadableStream<Uint8Array>({
      start(controller) { oldTerminal = controller; },
    }), { headers: { "x-astra-agent-interaction-api-major": "3" } }));
    const response = await stream();
    const oldRead = response.text();
    oldTerminal.enqueue(new TextEncoder().encode('data: {"type":"run_started","run_id":"run","index":0}\n\n'));
    await vi.waitFor(() => expect(getStore("owner").chats[0].activeRun?.source).toBe("stream"));
    const writer = getStore("owner").chats[0].streamProjection!.writer!;
    runtime.sdk.getSessionConversation.mockResolvedValue(conversation("next", status, "next request"));
    const detail = await getChatHydrated("owner", "session");
    expect(writer.signal.aborted).toBe(true);
    try {
      oldTerminal.enqueue(new TextEncoder().encode('data: {"type":"run_finished","run_id":"run","status":"failed","index":2}\n\n'));
      oldTerminal.close();
    } catch { /* Aborting the old reader can already have closed it. */ }
    await oldRead;
    expect(getChat("owner", "session")?.activeRun?.runId).toBe(status === "running" ? "next" : undefined);
    expect(detail?.messages[0].content).toBe("next request");
  }
});

it.each(["response", "503", "404"].flatMap(result => [false, true].map(legacy => ({ result, legacy }))))(
  "fences an older idle publication after a completed successor ($result, legacy=$legacy)", async ({ result, legacy }) => {
  const runtime = setup();
  const prior = conversation("run", "failed");
  if (legacy) prior.session = { ...session, metadata: { ...session.metadata, web_chat_id: "legacy-chat" } };
  runtime.sdk.listRuntimeSessions.mockResolvedValue({ sessions: [prior.session] });
  runtime.sdk.getSessionConversation.mockResolvedValue(prior);
  await getChatHydrated("owner", "session");
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const reading = new Promise<void>(resolve => { entered = resolve; });
  runtime.sdk.getSessionConversation.mockImplementationOnce(async () => {
    entered(); await pending;
    if (result !== "response") throw new AstraApiError(Number(result), "old read failed", "/sessions/session/transcript");
    return prior;
  });
  const older = getChatHydrated("owner", "session");
  await reading;
  runtime.sdk.getSessionConversation.mockResolvedValue({
    ...conversation("next", "completed", "newer completed input"), session: prior.session,
  });
  const newer = await getChatHydrated("owner", "session");
  release();
  await expect(older).resolves.toMatchObject({ messages: newer!.messages });
  expect(getChat("owner", newer!.chat.id)?.messages).toBe(newer!.messages);
  expect(runtime.sdk.listRuntimeSessions).toHaveBeenCalledTimes(1);
});

it("expires a real cancelling continuation through the existing Stop rules", async () => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  const assistantId = first!.activeRun!.assistantMessageId!;
  updateStreamingAssistantMessage("owner", "session", assistantId, { content: "partial answer" });
  await stopActiveRun("owner", "session", { sessionId: "session", runId: "run", assistantMessageId: assistantId },
    { skipSync: true, cancelTimeoutMs: 1 });
  const record = getStore("owner").chats[0];
  expect(record.streamProjection).toBeDefined();
  record.activeRun!.observedAt = new Date(Date.now() - 20_000).toISOString();
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.activeRun).toBeUndefined();
  expect(detail?.messages.at(-1)).toMatchObject({ content: "partial answer\n\nStopped.", status: "complete" });
  expect(getStore("owner").chats[0].locallyStoppedRuns?.run).toBeDefined();
});

it("returns the existing Web 404 after a known persisted Session is deleted remotely", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  runtime.sdk.getSessionConversation.mockRejectedValue(new AstraApiError(404, "Session missing", "/sessions/session/transcript"));
  const response = await GETDetail({} as never, { params: Promise.resolve({ chatId: "session" }) });
  expect(response.status).toBe(404);
  expect(getChat("owner", "session")).toBeNull();
});

it("joins a competing cold lookup by Session identity", async () => {
  const runtime = setup();
  const aliased = { ...session, metadata: { ...session.metadata, web_chat_id: "legacy-chat" } };
  runtime.sdk.listRuntimeSessions.mockResolvedValue({ sessions: [aliased] });
  runtime.sdk.getSessionConversation.mockResolvedValue({
    ...conversation("next", "completed", "newer completed input"), session: aliased,
  });
  runtime.sdk.getSessionConversation.mockImplementationOnce(async () => {
    await getChatHydrated("owner", "legacy-chat");
    return { ...conversation("run", "failed"), session: aliased };
  });
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.chat.id).toBe("legacy-chat");
  expect(detail?.messages.map(message => message.content)).toEqual(["newer completed input"]);
  expect(getStore("owner").chats).toHaveLength(1);
});

it.each([false, true])("retains a synchronous message through a concurrent detail GET (failure=%s)", async fail => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].model = "offering";
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const reading = new Promise<void>(resolve => { entered = resolve; });
  runtime.sdk.createRun.mockImplementationOnce(async () => {
    entered(); await pending;
    if (fail) throw new AstraApiError(503, "admission unavailable", "/runs");
    return { runId: "run", sessionId: "session", status: "running" };
  });
  const sending = POSTMessage({ json: async () => ({ content: "new request" }) } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  await reading;
  const during = await (await GETDetail({} as never, { params: Promise.resolve({ chatId: "session" }) })).json();
  const competing = await POSTMessage({ json: async () => ({ content: "duplicate request" }) } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  release();
  const response = await sending;
  expect(response.status).toBe(200);
  const result = await response.json();
  expect(during.messages.map((message: { content: string }) => message.content)).toContain("new request");
  expect(competing.status).toBe(409);
  expect(runtime.sdk.createRun).toHaveBeenCalledTimes(1);
  expect(getChat("owner", "session")?.pendingTurn).toBeUndefined();
  expect(result.assistantMessage.content).toContain(fail ? "admission unavailable" : "recovered answer");
  expect(getChat("owner", "session")?.messages.map(message => message.content)).toEqual([
    "original request", "new request", result.assistantMessage.content,
  ]);
});

it.each([20_000, 31 * 60_000])("retires a stopped terminal continuation after marker age %sms", async age => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  runtime.sdk.cancelRun.mockResolvedValue({});
  runtime.sdk.getRunStatus.mockRejectedValue(new Error("status temporarily unavailable"));
  await stopActiveRun("owner", "session", {
    sessionId: "session", runId: "run", assistantMessageId: first!.activeRun!.assistantMessageId!,
  }, { skipSync: true });
  const record = getStore("owner").chats[0];
  expect(record.activeRun?.status).toBe("cancelling");
  record.locallyStoppedRuns!.run = new Date(Date.now() - age).toISOString();
  record.activeRun!.observedAt = record.locallyStoppedRuns!.run;
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("run", "cancelled"));
  for (let i = 0; i < 2; i++) expect((await getChatHydrated("owner", "session"))?.activeRun).toBeUndefined();
});

it("rejects an invalid synchronous model before mutating messages or dispatching", async () => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  const before = JSON.stringify(getChat("owner", "session"));
  await expect(POSTMessage({ json: async () => ({ content: "must not be appended", options: { model: "   " } }) } as never,
    { params: Promise.resolve({ chatId: "session" }) })).rejects.toMatchObject({ code: "invalid_selection" });
  expect(JSON.stringify(getChat("owner", "session"))).toBe(before);
  expect(runtime.sdk.createRun).not.toHaveBeenCalled();
});

it("fences a detail read that began before synchronous admission", async () => {
  const runtime = setup();
  const prior = conversation("old", "completed");
  runtime.sdk.getSessionConversation.mockResolvedValue(prior);
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].model = "offering";
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const reading = new Promise<void>(resolve => { entered = resolve; });
  runtime.sdk.createRun.mockImplementationOnce(async () => {
    entered(); await pending;
    throw new AstraApiError(503, "admission unavailable", "/runs");
  });
  let sending!: ReturnType<typeof POSTMessage>;
  runtime.sdk.getSessionConversation.mockImplementationOnce(async () => {
    sending = POSTMessage({ json: async () => ({ content: "new request" }) } as never,
      { params: Promise.resolve({ chatId: "session" }) });
    await reading;
    return prior;
  });
  const detail = await getChatHydrated("owner", "session");
  release();
  await sending;
  expect(detail?.messages.map(message => message.content)).toContain("new request");
  expect(getChat("owner", "session")?.messages.at(-1)?.status).toBe("failed");
});

it.each(["delete", "successor", "stop"].flatMap(action => [false, true].map(fail => ({ action, fail }))))(
  "fences late synchronous output after $action (failure=$fail)", async ({ action, fail }) => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].model = "offering";
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const reading = new Promise<void>(resolve => { entered = resolve; });
  runtime.fetchResponse.mockImplementationOnce(async () => {
    entered(); await pending;
    if (fail) throw new Error("late read failure");
    return new Response('data: {"type":"text_delta","content":"late answer","index":0}\n\n' +
      'data: {"type":"run_finished","run_id":"run","status":"completed","index":1}\n\n');
  });
  const sending = POSTMessage({ json: async () => ({ content: "new request" }) } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  await reading;
  const active = getChat("owner", "session")!.activeRun!;
  expect(active.runId).toBe("run");
  if (action === "delete") await deleteChat("owner", "session");
  else if (action === "successor") {
    runtime.sdk.getSessionConversation.mockResolvedValue(conversation("next", "running", "successor request"));
    await getChatHydrated("owner", "session");
  } else {
    runtime.sdk.cancelRun.mockResolvedValue({});
    runtime.sdk.getRunStatus.mockResolvedValue({ runId: "run", sessionId: "session", status: "cancelled", eventsCount: 4 });
    await stopActiveRun("owner", "session", {
      sessionId: "session", runId: "run", assistantMessageId: active.assistantMessageId!,
    }, { skipSync: true });
  }
  const expected = JSON.stringify(getChat("owner", "session"));
  release();
  expect((await sending).status).toBe(502);
  expect(JSON.stringify(getChat("owner", "session"))).toBe(expected);
  expect(runtime.sdk.createRun).toHaveBeenCalledTimes(1);
});

it("does not observe a late admission after the Session is deleted", async () => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].model = "offering";
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const reading = new Promise<void>(resolve => { entered = resolve; });
  runtime.sdk.createRun.mockImplementationOnce(async () => {
    entered(); await pending;
    return { runId: "run", sessionId: "session", status: "running" };
  });
  const sending = POSTMessage({ json: async () => ({ content: "new request" }) } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  await reading;
  await deleteChat("owner", "session");
  release();
  expect((await sending).status).toBe(502);
  expect(runtime.fetchResponse).not.toHaveBeenCalled();
  expect(getChat("owner", "session")).toBeNull();
});

it("resumes an interrupted synchronous observation through the existing stream cursor", async () => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].model = "offering";
  runtime.fetchResponse.mockResolvedValueOnce(new Response(
    'data: {"type":"run_started","run_id":"run","index":0}\n\n' +
    'data: {"type":"text_delta","content":"partial","index":1}\n\n',
  ));
  const response = await POSTMessage({ json: async () => ({ content: "new request" }) } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  expect(response.status).toBe(502);
  expect(await response.json()).toMatchObject({ code: "stream_observation_interrupted" });
  expect(getChat("owner", "session")?.activeRun?.nextEventIndex).toBe(2);
  runtime.fetchResponse.mockResolvedValueOnce(new Response(
    'data: {"type":"text_delta","content":" tail","index":2}\n\n' +
    'data: {"type":"run_finished","run_id":"run","status":"completed","index":3}\n\n',
    { headers: { "x-astra-agent-interaction-api-major": "3" } },
  ));
  const resumed = await stream();
  expect(resumed.status).toBe(200);
  await resumed.text();
  expect(runtime.fetchResponse).toHaveBeenLastCalledWith("/chat/runs/run/stream?last_index=2", expect.anything());
  expect(runtime.sdk.createRun).toHaveBeenCalledTimes(1);
  expect(getChat("owner", "session")?.messages.at(-1)).toMatchObject({ content: "partial tail", status: "complete" });
  expect(getChat("owner", "session")?.activeRun).toBeUndefined();
});

it("starts the next turn while a completed stream is still enriching artifacts", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].model = "offering";
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const enriching = new Promise<void>(resolve => { entered = resolve; });
  runtime.sdk.listSessionArtifacts.mockResolvedValueOnce({ artifacts: [] });
  runtime.sdk.listSessionArtifacts.mockImplementationOnce(async () => {
    entered(); await pending; return { artifacts: [] };
  });
  const previous = await stream();
  const previousRead = previous.text();
  await enriching;
  expect(getChat("owner", "session")?.activeRun).toBeUndefined();
  const result = await POSTMessage({ json: async () => ({ content: "next request" }) } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  release();
  await previousRead;
  expect(result.status).toBe(200);
  expect(runtime.sdk.createRun).toHaveBeenCalledTimes(1);
  expect(getChat("owner", "session")?.messages.at(-1)).toMatchObject({ content: "recovered answer", status: "complete" });
});

it.each(["session", "run"])("settles a deadline during %s admission without an unresumable streaming message", async boundary => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].model = "offering";
  let entered!: () => void;
  const admitting = new Promise<void>(resolve => { entered = resolve; });
  let releaseSession!: () => void;
  if (boundary === "session") {
    getStore("owner").chats[0].backendSessionId = null;
    const pending = new Promise<void>(resolve => { releaseSession = resolve; });
    runtime.sdk.createRuntimeSession.mockImplementationOnce(async () => {
      entered(); await pending; return session;
    });
  } else {
    runtime.sdk.createRun.mockImplementationOnce(async (_request, init) => {
      entered();
      return new Promise((_, reject) => {
        init!.signal!.addEventListener("abort", () => reject(init!.signal!.reason), { once: true });
      });
    });
  }
  vi.useFakeTimers();
  try {
    const sending = POSTMessage({ json: async () => ({ content: "new request" }) } as never,
      { params: Promise.resolve({ chatId: "session" }) });
    await admitting;
    await vi.advanceTimersByTimeAsync(30_001);
    if (boundary === "session") releaseSession();
    const response = await sending;
    expect(response.status).toBe(200);
    expect((await response.json()).assistantMessage).toMatchObject({ status: "failed" });
    expect(getChat("owner", "session")?.activeRun).toBeUndefined();
    expect(getChat("owner", "session")?.messages.at(-1)?.status).toBe("failed");
    expect(runtime.fetchResponse).not.toHaveBeenCalled();
    expect(runtime.sdk.createRun).toHaveBeenCalledTimes(boundary === "session" ? 0 : 1);
  } finally {
    vi.useRealTimers();
  }
});

it.each(["waiting", "blocked"].flatMap(status => ["detail", "directory"].map(refresh => ({ status, refresh }))))(
  "resumes a $status parser after a terminal $refresh poll", async ({ status, refresh }) => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  runtime.fetchResponse.mockResolvedValueOnce(new Response([
    { type: "run_started", run_id: "run", index: 0 },
    { type: "text_delta", content: "partial", index: 1 },
    { type: status === "waiting" ? "run_waiting" : "run_blocked", run_id: "run", waiting_for: "tool_approval", index: 2 },
  ].map(event => `data: ${JSON.stringify(event)}\n\n`).join(""), {
    headers: { "x-astra-agent-interaction-api-major": "3" },
  }));
  await (await stream()).text();
  expect(getChat("owner", "session")?.activeRun).toMatchObject({ status, nextEventIndex: 3 });
  const completed = conversation("run", "completed");
  completed.items.push({ kind: "raw", session_id: "session", item_seq: 2, run_id: "run", role: "assistant", content: "partial final" });
  runtime.sdk.getSessionConversation.mockResolvedValue(completed);
  runtime.sdk.listRuns.mockResolvedValue({ runs: [{ runId: "run", sessionId: "session", parentRunId: null, depth: 0, status: "completed" }] });
  if (refresh === "detail") await getChatHydrated("owner", "session");
  else await listChats("owner", {});
  expect(getChat("owner", "session")?.activeRun).toMatchObject({ status, nextEventIndex: 3 });
  runtime.fetchResponse.mockResolvedValueOnce(new Response(
    'data: {"type":"run_resumed","run_id":"run","index":3}\n\n' +
    'data: {"type":"text_delta","content":" final","index":4}\n\n' +
    'data: {"type":"run_finished","run_id":"run","status":"completed","index":5}\n\n',
    { headers: { "x-astra-agent-interaction-api-major": "3" } },
  ));
  const resumed = await stream();
  expect(resumed.status).toBe(200);
  await resumed.text();
  expect(runtime.fetchResponse).toHaveBeenLastCalledWith("/chat/runs/run/stream?last_index=3", expect.anything());
  expect(runtime.sdk.createRun).not.toHaveBeenCalled();
  expect(getChat("owner", "session")?.messages.at(-1)).toMatchObject({ content: "partial final", status: "complete" });
  expect(getChat("owner", "session")?.activeRun).toBeUndefined();
});

it("retains a late admission acknowledgement after its deadline for stream recovery", async () => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].model = "offering";
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const admitting = new Promise<void>(resolve => { entered = resolve; });
  runtime.sdk.createRun.mockImplementationOnce(async () => {
    entered(); await pending;
    return { runId: "run", sessionId: "session", status: "running" };
  });
  vi.useFakeTimers();
  try {
    const sending = POSTMessage({ json: async () => ({ content: "new request" }) } as never,
      { params: Promise.resolve({ chatId: "session" }) });
    await admitting;
    await vi.advanceTimersByTimeAsync(30_001);
    release();
    expect((await sending).status).toBe(502);
    expect(runtime.fetchResponse).not.toHaveBeenCalled();
    expect(getChat("owner", "session")?.activeRun).toMatchObject({ runId: "run", nextEventIndex: 0 });
    await (await stream()).text();
    expect(runtime.sdk.createRun).toHaveBeenCalledTimes(1);
    expect(getChat("owner", "session")?.messages.at(-1)).toMatchObject({ content: "recovered answer", status: "complete" });
    expect(getChat("owner", "session")?.activeRun).toBeUndefined();
  } finally {
    vi.useRealTimers();
  }
});

it("publishes a terminal answer immediately when a paused parser cannot auto-attach", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  runtime.fetchResponse.mockResolvedValueOnce(new Response(
    'data: {"type":"text_delta","content":"partial","index":0}\n\n' +
    'data: {"type":"run_paused","run_id":"run","index":1}\n\n',
    { headers: { "x-astra-agent-interaction-api-major": "3" } },
  ));
  await (await stream()).text();
  expect(getChat("owner", "session")?.activeRun?.status).toBe("paused");
  const completed = conversation("run", "completed");
  completed.items.push({ kind: "raw", session_id: "session", item_seq: 2, run_id: "run", role: "assistant", content: "final answer" });
  runtime.sdk.getSessionConversation.mockResolvedValue(completed);
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.activeRun).toBeUndefined();
  expect(detail?.messages.at(-1)).toMatchObject({ content: "final answer", status: "complete" });
});

// Independent review counterexample: a real mutation must survive an older read.
it("preserves a model selection completed while detail recovery was reading", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  let resolveRead!: (value: RuntimeConversationResponse) => void;
  runtime.sdk.getSessionConversation.mockImplementationOnce(() => new Promise(resolve => { resolveRead = resolve; }));
  const reading = getChatHydrated("owner", "session");
  await vi.waitFor(() => expect(resolveRead).toBeDefined());
  const sdk = runtime.sdk as typeof runtime.sdk & { updateRuntimeSession: ReturnType<typeof vi.fn> };
  sdk.updateRuntimeSession = vi.fn().mockResolvedValue(session);
  await updateChatModel("owner", "session", "new-offering");
  expect(getChat("owner", "session")?.chat.model).toBe("new-offering");
  const old = conversation();
  old.session = { ...old.session, metadata: { source: "web_v1", current_model: "old-offering" } };
  resolveRead(old);
  await reading;
  expect(getChat("owner", "session")?.chat.model).toBe("new-offering");
});

it("preserves a completed workspace selection while an older detail snapshot returns", async () => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  let resolveRead!: (value: RuntimeConversationResponse) => void;
  runtime.sdk.getSessionConversation.mockImplementationOnce(() => new Promise(resolve => { resolveRead = resolve; }));
  const reading = getChatHydrated("owner", "session");
  await vi.waitFor(() => expect(resolveRead).toBeDefined());
  const sdk = runtime.sdk as typeof runtime.sdk & { updateRuntimeSession: ReturnType<typeof vi.fn> };
  sdk.updateRuntimeSession = vi.fn().mockResolvedValue(session);
  const selected = { kind: "edge_workspace" as const, edgeAgentId: "new-edge", cwd: "/workspace/new" };
  await updateChatWorkspaceSelection("owner", "session", selected);
  expect(getChat("owner", "session")?.workspaceSelection).toMatchObject(selected);
  const old = conversation("old", "completed");
  old.session = { ...old.session, metadata: { source: "web_v1", workspace_selection:
    { kind: "edge_workspace", edgeAgentId: "old-edge", cwd: "/workspace/old" } } };
  resolveRead(old);
  await reading;
  const edgeRuntime = runtime as typeof runtime & { get: ReturnType<typeof vi.fn> };
  edgeRuntime.get = vi.fn().mockResolvedValue({ edges: [
    { edge_agent_id: "new-edge", workspace_dir: "/workspace/new" },
    { edge_agent_id: "old-edge", workspace_dir: "/workspace/old" },
  ] });
  const response = await POSTStream(new Request("http://web.test/api/chats/session/stream", { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ content: "create a file in the selected workspace", options: { model: "offering" } }) }) as never,
    { params: Promise.resolve({ chatId: "session" }) });
  expect(response.status).toBe(200);
  await response.text();
  const requests = runtime.fetchResponse.mock.calls as unknown as Array<[string, { json?: { workspace_binding?: { root?: string } } }]>;
  const start = requests.find(([path]) => path === "/chat/stream");
  expect(start?.[1].json?.workspace_binding?.root).toBe("/workspace/new");
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function metadataWriter(runtime: ReturnType<typeof setup>) {
  const sdk = runtime.sdk as typeof runtime.sdk & { updateRuntimeSession: ReturnType<typeof vi.fn> };
  sdk.updateRuntimeSession = vi.fn().mockResolvedValue(session);
  return sdk.updateRuntimeSession;
}

it("commits an acknowledged model to the current shell after a reader replaces it", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  const write = deferred<typeof session>();
  const writer = metadataWriter(runtime).mockReturnValueOnce(write.promise);
  const changing = updateChatModel("owner", "session", "acknowledged-model");
  await vi.waitFor(() => expect(writer).toHaveBeenCalledTimes(1));
  await getChatHydrated("owner", "session");
  write.resolve(session);
  await changing;
  expect(getChat("owner", "session")?.chat.model).toBe("acknowledged-model");
});

it("does not roll back a successful workspace selection when an older write fails", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  const older = deferred<typeof session>();
  const writer = metadataWriter(runtime).mockReturnValueOnce(older.promise);
  const first = updateChatWorkspaceSelection("owner", "session", { kind: "edge_workspace", edgeAgentId: "older", cwd: "/older" });
  const failure = expect(first).rejects.toThrow("write failed");
  await vi.waitFor(() => expect(writer).toHaveBeenCalledTimes(1));
  expect(getChat("owner", "session")?.workspaceSelection).toBeUndefined();
  await updateChatWorkspaceSelection("owner", "session", { kind: "edge_workspace", edgeAgentId: "newer", cwd: "/newer" });
  older.reject(new Error("write failed"));
  await failure;
  expect(getChat("owner", "session")?.workspaceSelection).toMatchObject({ kind: "edge_workspace", cwd: "/newer" });
});

it.each(["delete", "rebind"])("does not publish an old metadata acknowledgement after %s", async action => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  const write = deferred<typeof session>();
  const writer = metadataWriter(runtime).mockReturnValueOnce(write.promise);
  const changing = updateChatModel("owner", "session", "obsolete");
  await vi.waitFor(() => expect(writer).toHaveBeenCalledTimes(1));
  if (action === "delete") await deleteChat("owner", "session");
  else getStore("owner").chats[0].backendSessionId = "replacement-session";
  write.resolve(session);
  expect(await changing).toBeNull();
  expect(getChat("owner", "session")?.chat.model).not.toBe("obsolete");
});

it.each(["detail", "directory"])("fences %s metadata across an A-B-A commit and archive/project changes", async source => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  const read = deferred<RuntimeConversationResponse>();
  const directory = deferred<{ sessions: typeof session[] }>();
  metadataWriter(runtime);
  await updateChatModel("owner", "session", "A");
  if (source === "detail") runtime.sdk.getSessionConversation.mockReturnValueOnce(read.promise);
  else runtime.sdk.listRuntimeSessions.mockReturnValueOnce(directory.promise);
  const reading = source === "detail" ? getChatHydrated("owner", "session") : listChats("owner", {});
  await vi.waitFor(() => expect(source === "detail" ? runtime.sdk.getSessionConversation : runtime.sdk.listRuntimeSessions).toHaveBeenCalledTimes(2));
  await updateChatModel("owner", "session", "B");
  await updateChatModel("owner", "session", "A");
  await archiveChat("owner", "session", true);
  const project = createProject("owner", { name: "project" });
  moveChat("owner", "session", project.id);
  const stale = conversation();
  stale.session = { ...session, metadata: { source: "web_v1", current_model: "stale" } };
  read.resolve(stale);
  directory.resolve({ sessions: [stale.session as typeof session] });
  await reading;
  const chat = getChat("owner", "session")!.chat;
  expect(chat.model).toBe("A");
  expect(chat.archivedAt).toBeTruthy();
  expect(chat.projectId).toBe(project.id);
});

it("keeps a warm parser on an explicitly omitted history and cold-loads a successor", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  const before = getChat("owner", "session")!.messages;
  runtime.sdk.getSessionConversation.mockResolvedValueOnce({ ...conversation(), history_loaded: false, items: [], raw_page: null, truncated: null });
  expect((await getChatHydrated("owner", "session"))?.messages).toBe(before);
  runtime.sdk.getSessionConversation.mockResolvedValueOnce(conversation("successor", "failed", "new accepted input"));
  expect((await getChatHydrated("owner", "session"))?.messages.map(item => item.content)).toEqual(["new accepted input", "Astra run failed."]);
});

it("binds the current shell after Session creation overlaps a metadata commit and reuses it on the next POST", async () => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("old", "completed"));
  await getChatHydrated("owner", "session");
  getStore("owner").chats[0].backendSessionId = null;
  const creation = deferred<typeof session>();
  runtime.sdk.createRuntimeSession.mockReturnValueOnce(creation.promise);
  const creating = ensureChatBackendSession("owner", "session", { model: "offering" });
  await vi.waitFor(() => expect(runtime.sdk.createRuntimeSession).toHaveBeenCalledTimes(1));
  const project = createProject("owner", { name: "new project" });
  moveChat("owner", "session", project.id);
  creation.resolve(session);
  expect(await creating).toBe("session");
  expect(getChat("owner", "session")?.session?.backendSessionId).toBe("session");
  expect(getChat("owner", "session")?.chat.projectId).toBe(project.id);
  expect(await ensureChatBackendSession("owner", "session", { model: "offering" })).toBe("session");
  const response = await POSTStream(new Request("http://web.test/api/chats/session/stream", {
    method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ content: "hello again", options: { model: "offering" } }),
  }) as never, { params: Promise.resolve({ chatId: "session" }) });
  expect(response.status).toBe(200);
  await response.text();
  expect(runtime.sdk.createRuntimeSession).toHaveBeenCalledTimes(1);
  const calls = runtime.fetchResponse.mock.calls as unknown as Array<[string, { json?: { session_id?: string } }]>;
  expect(calls.find(([path]) => path === "/chat/stream")?.[1].json?.session_id).toBe("session");
});

it("rejects an old missing-Session result after A-B-A metadata commits", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  metadataWriter(runtime);
  await updateChatModel("owner", "session", "A");
  const read = deferred<RuntimeConversationResponse>();
  runtime.sdk.getSessionConversation.mockReturnValueOnce(read.promise);
  const reading = getChatHydrated("owner", "session");
  await vi.waitFor(() => expect(runtime.sdk.getSessionConversation).toHaveBeenCalledTimes(2));
  await updateChatModel("owner", "session", "B");
  await updateChatModel("owner", "session", "A");
  read.reject(new AstraApiError(404, "Old Session snapshot was missing", "/sessions/session/transcript"));
  expect((await reading)?.chat.model).toBe("A");
  expect(getChat("owner", "session")?.chat.model).toBe("A");
});

it("uses the admitted client throughout chat detail hydration", async () => {
  const runtime = setup();
  vi.mocked(getRuntimeClient).mockClear().mockRejectedValue(new Error("unexpected second authentication"));
  vi.mocked(requireRuntimeClient).mockClear().mockRejectedValue(new Error("unexpected second authentication"));
  const response = await GETDetail({} as never, { params: Promise.resolve({ chatId: "session" }) });
  expect(response.status).toBe(200);
  expect((await response.json()).messages[0].content).toBe("original request");
  expect(runtime.sdk.getSessionConversation).toHaveBeenCalled();
  expect(getRuntimeClient).not.toHaveBeenCalled();
  expect(requireRuntimeClient).not.toHaveBeenCalled();
});

it("preserves an authentication outage when opening an existing stream", async () => {
  setup();
  vi.mocked(requireRuntimeUser).mockResolvedValue({ user: null, client: null,
    response: new Response(JSON.stringify({ error: "service unavailable" }), { status: 503 }) } as never);
  const response = await stream();
  expect(response.status).toBe(503);
  expect(await response.json()).toEqual({ error: "service unavailable" });
});

it("uses the admitted client to cancel and reconcile a completed Stop", async () => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  runtime.sdk.cancelRun.mockResolvedValue({});
  runtime.sdk.getRunStatus.mockResolvedValue({ runId: "run", sessionId: "session", status: "cancelled", eventsCount: 4 });
  vi.mocked(getRuntimeClient).mockClear().mockRejectedValue(new Error("unexpected second authentication"));
  vi.mocked(requireRuntimeClient).mockClear().mockRejectedValue(new Error("unexpected second authentication"));
  const result = await stopActiveRun("owner", "session", {
    sessionId: "session", runId: "run", assistantMessageId: first!.activeRun!.assistantMessageId!,
  }, { skipSync: true, client: runtime });
  expect(result?.cancelPending).toBe(false);
  expect(runtime.sdk.cancelRun).toHaveBeenCalledWith("run");
  expect(runtime.sdk.getRunStatus).toHaveBeenCalledWith("run");
  expect(getRuntimeClient).not.toHaveBeenCalled();
  expect(requireRuntimeClient).not.toHaveBeenCalled();
});

it("clears multiple archived chats without resolving the admitted session again", async () => {
  const runtime = setup();
  runtime.sdk.listRuntimeSessions.mockResolvedValue({ sessions: [
    { ...session, session_id: "archived-a", status: "archived" },
    { ...session, session_id: "archived-b", status: "archived" },
  ] });
  runtime.sdk.listRuns.mockResolvedValue({ runs: [] });
  vi.mocked(getRuntimeClient).mockClear().mockRejectedValue(new Error("unexpected second authentication"));
  vi.mocked(requireRuntimeClient).mockClear().mockRejectedValue(new Error("unexpected second authentication"));
  const response = await DELETEArchived({ nextUrl: new URL("http://web.test/api/chats?archived=true") } as never);
  expect(response.status).toBe(200);
  expect(await response.json()).toEqual({ deleted: 2 });
  expect(runtime.sdk.deleteSession.mock.calls.map(call => call[0]).sort()).toEqual(["archived-a", "archived-b"]);
  expect(getRuntimeClient).not.toHaveBeenCalled();
  expect(requireRuntimeClient).not.toHaveBeenCalled();
});

// @vitest-environment node
vi.mock("@/lib/api/auth-guard", () => ({ requireRuntimeUser: vi.fn() }));
vi.mock("@/lib/runtime-client/server", () => ({
  WebRuntimeClient: class {}, getRuntimeClient: vi.fn(), requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getRuntimeClient, requireRuntimeClient } from "@/lib/runtime-client/server";
import { getChat, getChatHydrated, getStore, queueDeferredRunInput, stopActiveRun, updateStreamingAssistantMessage } from "@/lib/api/web-store";
import { GET as GETDetail } from "@/app/api/chats/[chatId]/route";
import { GET } from "@/app/api/chats/[chatId]/stream/route";
import { AstraApiError, type RuntimeConversationResponse } from "@astra/sdk";

const session = { session_id: "session", created_at: "2026-10-25T03:00:00Z", status: "active", metadata: { source: "web_v1" } };
function conversation(runId = "run", status = "running", content = "original request"): RuntimeConversationResponse {
  return { kind: "conversation", session,
    selected_root: { run_id: runId, status, waiting_for: null, created_at: "2026-10-25T03:00:00.123456Z" },
    items: [{ kind: "opening", id: `input:${runId}`, run_id: runId, source_event_id: `source:${runId}`,
      created_at: "2026-10-25T03:00:00.123456Z", content }],
    raw_page: { next_before_seq: null, has_more: false }, truncated: false };
}
function setup() {
  globalThis.__astraWebStores = undefined;
  vi.mocked(requireRuntimeUser).mockResolvedValue({ user: { user_id: "owner", display_name: null }, response: null } as never);
  const sdk = {
    listRuntimeSessions: vi.fn().mockResolvedValue({ sessions: [session] }),
    listRuns: vi.fn().mockResolvedValue({ runs: [{ runId: "run", sessionId: "session", parentRunId: null, depth: 0, status: "running" }] }),
    getSessionConversation: vi.fn().mockResolvedValue(conversation()),
    getRunStatus: vi.fn().mockResolvedValue({ runId: "run", sessionId: "session", status: "running", eventsCount: 4 }),
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
  const runtime = { sdk, fetchResponse };
  vi.mocked(getRuntimeClient).mockResolvedValue(runtime as never);
  vi.mocked(requireRuntimeClient).mockResolvedValue(runtime as never);
  return runtime;
}
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

it("retains the exact warm parser and deferred guidance when the database terminal arrives first", async () => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  updateStreamingAssistantMessage("owner", "session", first!.activeRun!.assistantMessageId!, { content: "first partial" });
  const next = await queueDeferredRunInput("owner", "session", { content: "guidance", pendingMessageId: "guidance-1" });
  updateStreamingAssistantMessage("owner", "session", next!.assistantMessage.id, { content: "second partial" });
  const before = getChat("owner", "session")!.messages;
  const parser = getStore("owner").chats[0].streamProjection;
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("run", "completed"));
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
  expect(detail?.messages.map(message => message.content)).toEqual([" \n你好 "]);
  expect(detail?.activeRun).toBeUndefined();
  expect(runtime.sdk.getRunStatus).not.toHaveBeenCalled();
});

it("discovers an externally completed successor from a nonempty opening-only idle view", async () => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("run", "failed"));
  await getChatHydrated("owner", "session");
  const next = conversation("next", "completed", "next request");
  next.items.unshift(conversation().items[0]);
  next.items.push({ kind: "raw", session_id: "session", item_seq: 2, run_id: "next", role: "assistant", content: "new answer" });
  runtime.sdk.getSessionConversation.mockResolvedValue(next);
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "next request", "new answer"]);
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
  expect(runtime.sdk.getSessionConversation.mock.calls).toEqual([["session", 200], ["session", 200]]);
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

it.each(["response", "503", "404"])("fences an older idle publication after a completed successor (%s)", async result => {
  const runtime = setup();
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("run", "failed"));
  await getChatHydrated("owner", "session");
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const reading = new Promise<void>(resolve => { entered = resolve; });
  runtime.sdk.getSessionConversation.mockImplementationOnce(async () => {
    entered(); await pending;
    if (result !== "response") throw new AstraApiError(Number(result), "old read failed", "/sessions/session/transcript");
    return conversation("run", "failed");
  });
  const older = getChatHydrated("owner", "session");
  await reading;
  runtime.sdk.getSessionConversation.mockResolvedValue(conversation("next", "completed", "newer completed input"));
  const newer = await getChatHydrated("owner", "session");
  release();
  await expect(older).resolves.toMatchObject({ messages: newer!.messages });
  expect(getChat("owner", "session")?.messages).toBe(newer!.messages);
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

// @vitest-environment node
vi.mock("@/lib/api/auth-guard", () => ({ requireRuntimeUser: vi.fn() }));
vi.mock("@/lib/runtime-client/server", () => ({
  WebRuntimeClient: class {}, getRuntimeClient: vi.fn(), requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getRuntimeClient, requireRuntimeClient } from "@/lib/runtime-client/server";
import { getChat, getChatHydrated, getStore, queueDeferredRunInput, updateStreamingAssistantMessage } from "@/lib/api/web-store";
import { GET } from "@/app/api/chats/[chatId]/stream/route";
import type { RunStatus } from "@astra/sdk";

function recordedRun(runId = "run", status = "running", content = "original request", createdAt = "2026-10-25T03:00:00Z"): RunStatus {
  return { runId, sessionId: "session", rootRunId: runId, parentRunId: null, depth: 0, status, eventsCount: 4,
    openingInput: { original_input: content, source_event_id: `input:${runId}`, created_at: createdAt } };
}

function setup() {
  globalThis.__astraWebStores = undefined;
  vi.mocked(requireRuntimeUser).mockResolvedValue({ user: { user_id: "owner", display_name: null }, response: null } as never);
  const sdk = {
    listRuntimeSessions: vi.fn().mockResolvedValue({ sessions: [{ session_id: "session", status: "active", metadata: { source: "web_v1" } }] }),
    listRuns: vi.fn(async (_options?: { limit: number; includeInputForSession?: string }) => ({ runs: [recordedRun()] })),
    getSessionTranscript: vi.fn().mockResolvedValue({ items: [] }),
    getRunStatus: vi.fn().mockResolvedValue({ runId: "run", sessionId: "session", status: "running", eventsCount: 4 }),
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

it("hydrates an opening-only Run and actually resumes its stream to completion", async () => {
  const runtime = setup();
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.activeRun?.nextEventIndex).toBe(0);
  const response = await GET({ nextUrl: new URL("http://web.test/api/chats/session/stream?runId=run&last_index=0") } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  expect(response.status).toBe(200);
  expect(await response.text()).toContain("recovered answer");
  expect(runtime.fetchResponse).toHaveBeenCalledWith("/chat/runs/run/stream?last_index=0", expect.objectContaining({ method: "GET" }));
  const completed = getChat("owner", "session");
  expect(completed?.messages.map(message => message.content)).toEqual(["original request", "recovered answer"]);
  expect(completed?.messages[1].status).toBe("complete");
  expect(completed?.activeRun).toBeUndefined();
});

it("preserves the complete live projection across deferred guidance and a terminal commit window", async () => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  updateStreamingAssistantMessage("owner", "session", first!.activeRun!.assistantMessageId!, { content: "first partial" });
  const next = await queueDeferredRunInput("owner", "session", { content: "guidance", pendingMessageId: "guidance-1" });
  updateStreamingAssistantMessage("owner", "session", next!.assistantMessage.id, { content: "second partial" });
  const before = getChat("owner", "session")!.messages;
  // Run listing can predate terminal commit; a warm parser must consume its
  // real terminal instead of appending a duplicate to committed history.
  runtime.sdk.getSessionTranscript.mockResolvedValue({ items: [
    { item_seq: 1, run_id: "run", role: "user", content: "original request" },
    { item_seq: 2, run_id: "run", role: "assistant", content: "final answer" },
  ] } as never);
  const hydrated = await getChatHydrated("owner", "session");
  expect(hydrated?.messages).toBe(before);
  expect(hydrated?.messages.map(message => message.content)).toEqual(["original request", "first partial", "guidance", "second partial"]);
  expect(hydrated?.activeRun?.assistantMessageId).toBe(next!.assistantMessage.id);
  expect(hydrated?.activeRun?.nextEventIndex).toBe(4);
  expect(runtime.sdk.getSessionTranscript).toHaveBeenCalledTimes(1);
  expect(runtime.sdk.getRunStatus).toHaveBeenCalledTimes(2);
  expect(runtime.sdk.listRuns).toHaveBeenLastCalledWith({ limit: 200, includeInputForSession: "session" });
});

it("keeps a warm accepted turn until its real Failed stream terminal even when polling finishes first", async () => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  const before = first!.messages;
  const { openingInput: _input, ...compact } = recordedRun("run", "failed");
  runtime.sdk.listRuns.mockResolvedValue({ runs: [compact] } as never);
  const hydrated = await getChatHydrated("owner", "session");
  expect(hydrated?.messages).toBe(before);
  expect(hydrated?.activeRun?.runId).toBe("run");
  expect(runtime.sdk.listRuns).toHaveBeenLastCalledWith({ limit: 200, includeInputForSession: "session" });
  runtime.fetchResponse.mockResolvedValue(new Response([
    { type: "run_error", run_id: "run", error: "capacity timeout", error_code: "run_admission_timeout", error_kind: "server_error", index: 1 },
    { type: "run_finished", run_id: "run", status: "failed", index: 2 },
  ].map(event => `data: ${JSON.stringify(event)}\n\n`).join(""), {
    headers: { "x-astra-agent-interaction-api-major": "3" },
  }));
  const response = await GET({ nextUrl: new URL("http://web.test/api/chats/session/stream?runId=run&last_index=0") } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  await response.text();
  const failed = getChat("owner", "session");
  expect(failed?.messages[0].content).toBe("original request");
  expect(failed?.messages[1].status).toBe("failed");
  expect(failed?.activeRun).toBeUndefined();
});

it("discards hydration when deferred input replaces the assistant within the same Run during await", async () => {
  const runtime = setup();
  runtime.sdk.getSessionTranscript.mockImplementationOnce(async () => {
    const chat = getStore("owner").chats[0];
    chat.messages.push({ id: "original", role: "user", content: "original request", createdAt: new Date().toISOString() });
    await queueDeferredRunInput("owner", "session", { content: "guidance", pendingMessageId: "guidance-1" });
    return { items: [] };
  });
  const hydrated = await getChatHydrated("owner", "session");
  expect(hydrated?.messages.map(message => message.content)).toEqual(["original request", "guidance", ""]);
  expect(hydrated?.activeRun?.nextEventIndex).toBe(4);
  expect(runtime.sdk.getRunStatus).toHaveBeenCalledTimes(1);
});

it.each(["failed", "cancelled"])("keeps accepted input when an opening-only Run is %s", async status => {
  for (const duringRecovery of [false, true]) {
    const runtime = setup();
    runtime.sdk.listRuns.mockResolvedValue({ runs: [recordedRun("run", duringRecovery ? "running" : status)] });
    runtime.sdk.getRunStatus.mockResolvedValue({ runId: "run", sessionId: "session", status, eventsCount: 4 });
    const detail = await getChatHydrated("owner", "session");
    expect(detail?.messages.map(message => message.content)).toEqual(["original request"]);
    expect(detail?.activeRun).toBeUndefined();
    expect(runtime.sdk.getRunStatus).toHaveBeenCalledTimes(duringRecovery ? 1 : 0);
    expect(runtime.sdk.getSessionTranscript).toHaveBeenCalledTimes(duringRecovery ? 2 : 1);
  }
});

it("merges input identity and admission order without changing raw history order", async () => {
  const runtime = setup();
  const runs = [
    recordedRun("d", "completed", "last request", "2026-10-25T03:00:00.300Z"),
    recordedRun("c", "cancelled", "third request", "2026-10-25T03:00:00.200Z"),
    recordedRun("a", "completed", "same request", "2026-10-25T03:00:00.100Z"),
    recordedRun("b", "failed", "same request", "2026-10-25T03:00:00.200Z"),
  ];
  const child = { ...recordedRun("child", "failed", "private child task"), parentRunId: "d", depth: 1 };
  const foreign = { ...recordedRun("foreign", "failed", "another Session"), sessionId: "other-session" };
  runtime.sdk.listRuns.mockResolvedValue({ runs: [...runs, child, foreign] } as never);
  runtime.sdk.getSessionTranscript.mockResolvedValue({ items: [
    { item_seq: 1, run_id: "a", source_event_id: "input:a", role: "user", content: "same request" },
    { item_seq: 2, run_id: "a", role: "assistant", content: "answer a" },
    { item_seq: 3, run_id: null, role: "user", content: "imported", created_at: "2026-10-25T03:00:00.150Z" },
    { item_seq: 4, run_id: "d", source_event_id: "input:d", role: "user", content: "last request" },
    { item_seq: 5, run_id: "d", source_event_id: "guidance:d", role: "user", content: "applied guidance" },
    { item_seq: 6, run_id: "d", role: "assistant", content: "answer d" },
  ] } as never);
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages.map(message => message.content)).toEqual([
    "same request", "answer a", "imported", "same request", "third request", "last request", "applied guidance", "answer d",
  ]);
  expect(runtime.sdk.getRunStatus).not.toHaveBeenCalled();
});

it("requests only the selected Session input across list pages and bounds visible history", async () => {
  const runtime = setup();
  const runs = Array.from({ length: 205 }, (_, i) => recordedRun(`run-${i}`, "failed", `request ${i}`,
    new Date(Date.UTC(2026, 9, 25, 3, 0, i)).toISOString()));
  const firstPage = { runs: runs.slice(5).reverse(), limit: 200, nextCursor: { updatedAt: "cursor-time", runId: "run-5" } };
  const lastPage = { runs: runs.slice(0, 5).reverse(), limit: 200 };
  runtime.sdk.listRuns.mockResolvedValueOnce(firstPage).mockResolvedValueOnce(lastPage);
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages).toHaveLength(200);
  expect(detail?.messages[0].content).toBe("request 5");
  expect(detail?.messages.at(-1)?.content).toBe("request 204");
  expect(runtime.sdk.listRuns).toHaveBeenNthCalledWith(1, { limit: 200, includeInputForSession: "session" });
  expect(runtime.sdk.listRuns).toHaveBeenNthCalledWith(2, { limit: 200, includeInputForSession: "session", cursor: firstPage.nextCursor });
  expect(runtime.sdk.getRunStatus).not.toHaveBeenCalled();
});

it("discards status recovery when deferred input replaces its binding during await", async () => {
  const runtime = setup();
  runtime.sdk.getRunStatus.mockImplementationOnce(async () => {
    getStore("owner").chats[0].messages.push({ id: "original", role: "user", content: "original request", createdAt: new Date().toISOString() });
    await queueDeferredRunInput("owner", "session", { content: "guidance", pendingMessageId: "guidance-1" });
    return { runId: "run", sessionId: "session", status: "running", eventsCount: 4 };
  });
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "guidance", ""]);
  expect(detail?.activeRun?.nextEventIndex).toBe(4);
});

it("keeps the current accepted input when applied guidance fills the raw history window", async () => {
  const runtime = setup();
  runtime.sdk.listRuns.mockResolvedValue({ runs: [recordedRun("run", "paused")] });
  runtime.sdk.getRunStatus.mockResolvedValue({ runId: "run", sessionId: "session", status: "paused", eventsCount: 4 });
  runtime.sdk.getSessionTranscript.mockResolvedValue({ items: Array.from({ length: 200 }, (_, i) => ({
    item_seq: i + 2, run_id: "run", source_event_id: `guidance:${i}`, role: "user", content: `guidance ${i}`,
  })) } as never);
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages).toHaveLength(201); // bounded history plus the active assistant
  expect(detail?.messages[0].content).toBe("original request");
  expect(detail?.messages.at(-2)?.content).toBe("guidance 199");
});

it("discards a stale recovery after another recovery replaces the same assistant's parser", async () => {
  const runtime = setup();
  const first = await getChatHydrated("owner", "session");
  getStore("owner").chats[0].streamProjection!.state.protocolError = true;
  runtime.sdk.getRunStatus.mockImplementationOnce(async () => {
    await getChatHydrated("owner", "session");
    updateStreamingAssistantMessage("owner", "session", first!.activeRun!.assistantMessageId!, { content: "newer recovery" });
    return { runId: "run", sessionId: "session", status: "running", eventsCount: 4 };
  });
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "newer recovery"]);
  expect(detail?.activeRun?.assistantMessageId).toBe(first!.activeRun!.assistantMessageId);
});

it("recovers a successor selected by the list even when the prior Run still has a warm parser", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  const runs = [recordedRun("next-run", "running", "next request", "2026-10-25T03:01:00Z"), recordedRun("run", "failed")];
  runtime.sdk.listRuns.mockImplementation(async options => ({ runs: runs.map(run =>
    options?.includeInputForSession ? run : { ...run, openingInput: undefined }) }));
  runtime.sdk.getRunStatus.mockResolvedValue({ runId: "next-run", sessionId: "session", status: "running", eventsCount: 4 });
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.activeRun?.runId).toBe("next-run");
  expect(detail?.activeRun?.nextEventIndex).toBe(0);
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "next request", ""]);
});

it("discovers a new Run from loaded inactive history using an opt-in-respecting list", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  const response = await GET({ nextUrl: new URL("http://web.test/api/chats/session/stream?runId=run&last_index=0") } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  await response.text();
  expect(getChat("owner", "session")?.activeRun).toBeUndefined();
  const runs = [recordedRun("next-run", "running", "next request", "2026-10-25T03:01:00Z"), recordedRun("run", "completed")];
  runtime.sdk.listRuns.mockImplementation(async options => ({ runs: runs.map(run =>
    options?.includeInputForSession ? run : { ...run, openingInput: undefined }) }));
  runtime.sdk.getSessionTranscript.mockResolvedValue({ items: [
    { item_seq: 1, run_id: "run", source_event_id: "input:run", role: "user", content: "original request" },
    { item_seq: 2, run_id: "run", role: "assistant", content: "recovered answer" },
  ] } as never);
  runtime.sdk.getRunStatus.mockResolvedValue({ runId: "next-run", sessionId: "session", status: "running", eventsCount: 4 });
  const detail = await getChatHydrated("owner", "session");
  expect(detail?.activeRun?.runId).toBe("next-run");
  expect(detail?.activeRun?.nextEventIndex).toBe(0);
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "recovered answer", "next request", ""]);
});

it("retires the old stream writer before publishing a successor during hydration", async () => {
  const runtime = setup();
  await getChatHydrated("owner", "session");
  let oldTerminal!: ReadableStreamDefaultController<Uint8Array>;
  let cancelled = false;
  runtime.fetchResponse.mockResolvedValueOnce(new Response(new ReadableStream<Uint8Array>({
    start(controller) { oldTerminal = controller; },
    cancel() { cancelled = true; },
  }), { headers: { "x-astra-agent-interaction-api-major": "3" } }));
  const response = await GET({ nextUrl: new URL("http://web.test/api/chats/session/stream?runId=run&last_index=0") } as never,
    { params: Promise.resolve({ chatId: "session" }) });
  const oldRead = response.text();
  runtime.sdk.listRuns.mockResolvedValue({ runs: [
    recordedRun("next-run", "running", "next request", "2026-10-25T03:01:00Z"), recordedRun("run", "failed"),
  ] });
  runtime.sdk.getRunStatus.mockResolvedValue({ runId: "next-run", sessionId: "session", status: "running", eventsCount: 4 });
  let release!: () => void;
  let entered!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const reading = new Promise<void>(resolve => { entered = resolve; });
  runtime.sdk.getSessionTranscript.mockImplementationOnce(async () => { entered(); await pending; return { items: [] }; });
  const hydration = getChatHydrated("owner", "session");
  await reading;
  if (!cancelled) {
    oldTerminal.enqueue(new TextEncoder().encode([
      { type: "run_error", run_id: "run", error: "old terminal", error_code: "server_error", error_kind: "server_error", index: 1 },
      { type: "run_finished", run_id: "run", status: "failed", index: 2 },
    ].map(event => `data: ${JSON.stringify(event)}\n\n`).join("")));
    oldTerminal.close();
  }
  await oldRead;
  const selected = getChat("owner", "session")?.activeRun?.runId;
  release();
  const detail = await hydration;
  expect(selected).toBe("next-run");
  expect(detail?.activeRun?.runId).toBe("next-run");
  expect(detail?.activeRun?.nextEventIndex).toBe(0);
  expect(detail?.messages.map(message => message.content)).toEqual(["original request", "next request", ""]);
});

// @vitest-environment node
vi.mock("@/lib/api/auth-guard", () => ({ requireRuntimeUser: vi.fn() }));
vi.mock("@/lib/runtime-client/server", async importOriginal => ({
  ...(await importOriginal<typeof import("@/lib/runtime-client/server")>()),
  getRuntimeClient: vi.fn().mockResolvedValue(null), requireRuntimeClient: vi.fn(),
}));
vi.mock("@/lib/runtime-client", async () => ({
  ...(await import("@/lib/runtime-client/errors")), requireRuntimeClient: vi.fn(),
}));

import { GET, POST } from "@/app/api/chats/[chatId]/stream/route";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { requireRuntimeClient } from "@/lib/runtime-client";
import { getStore, getChat, claimChatStreamProjection, withChatStreamProjection,
  releaseChatStreamProjection, beginStreamingMessage, queueDeferredRunInput, resumeActiveRun, stopActiveRun, updateStreamingAssistantMessage, setChatActiveRun, getChatHydrated } from "@/lib/api/web-store";
import { WebRuntimeClient, getRuntimeClient as getStoreRuntimeClient, requireRuntimeClient as requireStoreRuntimeClient } from "@/lib/runtime-client/server";
import { streamExistingChatRun, streamChatMessage } from "@/lib/api/chats";
import { isNotFoundError } from "@/lib/api/errors";

function chat(owner = "owner", id = "chat") {
  const record = { id, title: "Chat", projectId: null, createdAt: "2026-10-21T00:00:00Z",
    lastMessageAt: "2026-10-21T00:00:00Z", backendSessionId: id === "chat" ? "session" : id,
    messages: [{ id: "assistant", role: "assistant" as const, content: "", status: "streaming" as const,
      createdAt: "2026-10-21T00:00:00Z" }] };
  getStore(owner).chats.push(record);
}

function backend(events: Array<Record<string, unknown>>, pending = false, readError?: Error) {
  let emitted = false;
  let finish: (() => void) | undefined;
  const cancel = vi.fn(() => { finish?.(); });
  const releaseLock = vi.fn();
  return { ok: true, headers: new Headers({ "x-astra-agent-interaction-api-major": "3" }),
    body: { getReader: () => ({ cancel, releaseLock, read: vi.fn(async () => {
      if (!emitted) {
        emitted = true;
        return { done: false, value: new TextEncoder().encode(events.map(event => `data: ${JSON.stringify(event)}\n\n`).join("")) };
      }
      if (readError) throw readError;
      if (pending) await new Promise<void>(resolve => { finish = resolve; });
      return { done: true, value: undefined };
    }) }) }, cancel, releaseLock };
}

function runtime(response: ReturnType<typeof backend>) {
  const value = new WebRuntimeClient({ mode: "live", apiUrl: "http://runtime.test", accessToken: "test-token", hasAccessToken: true,
    source: "none", demoMode: false, hasRefreshToken: false, message: "Test runtime" });
  vi.spyOn(value.sdk, "listSessionArtifacts").mockResolvedValue({ artifacts: [] } as never);
  vi.spyOn(value.sdk, "createRuntimeSession").mockResolvedValue({ session_id: "session" } as never);
  vi.spyOn(value, "fetchResponse").mockResolvedValue(response as never);
  const previous = vi.mocked(requireRuntimeUser).getMockImplementation();
  vi.mocked(requireRuntimeUser).mockImplementation(async () => ({ ...(await previous!()), client: value }) as never);
  vi.mocked(requireRuntimeClient).mockResolvedValue(value);
  vi.mocked(requireStoreRuntimeClient).mockResolvedValue(value);
  return value;
}

let assistantId = "assistant";
const payload = { content: "start", options: { model: "fixture", thinking: true, webSearch: false, activeSkills: [] } };
async function startPost(id = "chat") {
  const response = await POST(new Request(`http://web.test/api/chats/${id}/stream`, {
    method: "POST", body: JSON.stringify(payload),
  }) as never, { params: Promise.resolve({ chatId: id }) });
  assistantId = getChat("owner", id)!.messages.at(-1)!.id;
  return response;
}
function selectedMessage() { return getChat("owner", "chat")!.messages.find(message => message.id === assistantId)!; }

async function attach(index?: number, id = "chat") {
  const nextUrl = new URL(`http://web.test/api/chats/${id}/stream?runId=root&assistantMessageId=${assistantId}${index === undefined ? "" : `&last_index=${index}`}`);
  return GET({ nextUrl } as never, { params: Promise.resolve({ chatId: id }) });
}

const bootstrap = { type: "session_info", session_id: "session", run_id: "root" };
const failure = { type: "run_error", run_id: "root", error: "Save failed", error_code: "canonical_persistence_failed" };
const terminal = { type: "run_finished", run_id: "root", status: "failed", error: "Save failed" };

async function prefix(text: string, reasoning?: string, failed = false, lifecycle?: Record<string, unknown>) {
  const first = backend([bootstrap, { type: "text_delta", content: text },
    ...(reasoning ? [{ type: "reasoning_delta", content: reasoning }] : []),
    { type: "run_input_queued", run_id: "root", index: 7 }, ...(lifecycle ? [{ ...lifecycle, index: 8 }] : []), ...(failed ? [failure] : [])], true);
  runtime(first);
  const response = await startPost();
  const reader = response.body!.getReader();
  await reader.read();
  await vi.waitFor(() => expect(getChat("owner", "chat")?.activeRun?.nextEventIndex ??
    getStore("owner").chats.find(item => item.id === "chat")?.streamProjection?.state.nextEventIndex).toBe(lifecycle ? 9 : 8));
  await reader.cancel();
  await vi.waitFor(() => expect(first.releaseLock).toHaveBeenCalled());
  return first;
}

beforeEach(() => {
  globalThis.__astraWebStores = {};
  assistantId = "assistant";
  vi.clearAllMocks();
  vi.mocked(getStoreRuntimeClient).mockResolvedValue(null);
  vi.mocked(requireRuntimeUser).mockResolvedValue({ user: { user_id: "owner" }, response: null } as never);
  chat();
});
afterEach(() => vi.restoreAllMocks());

it.each(["EOF", "read reset", "fetch reset", "HTTP unavailable"])("preserves a disconnected projection on %s and continues the same run", async transport => {
  await prefix("Partial answer ", "Saved reasoning");
  const next = backend([bootstrap], false, transport === "read reset" ? new Error("socket reset") : undefined);
  const client = runtime(next);
  if (transport === "fetch reset") vi.mocked(client.fetchResponse).mockRejectedValue(new TypeError("fetch failed"));
  if (transport === "HTTP unavailable") vi.mocked(client.fetchResponse).mockResolvedValue(new Response("Unavailable", { status: 503 }));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(8));
  const onDone = vi.fn();
  await expect(streamExistingChatRun("chat", "root", { onDone },
    { nextEventIndex: 8, assistantMessageId: assistantId })).rejects
    .toMatchObject({ code: "stream_observation_interrupted" });
  expect(selectedMessage()).toMatchObject({ content: "Partial answer", reasoning: "Saved reasoning", status: "streaming" });
  expect(getChat("owner", "chat")?.activeRun).toMatchObject({ runId: "root", nextEventIndex: 8 });
  expect(onDone).not.toHaveBeenCalled();

  runtime(backend([bootstrap, { type: "text_delta", content: "continued", index: 8 },
    { type: "run_finished", run_id: "root", status: "completed", index: 9 }]));
  vi.mocked(globalThis.fetch).mockResolvedValue(await attach(8));
  await expect(streamExistingChatRun("chat", "root", { onDone },
    { nextEventIndex: 8, assistantMessageId: assistantId })).resolves.toBe("Partial answer continued");
  expect(onDone).toHaveBeenCalledExactlyOnceWith("Partial answer continued");
  expect(selectedMessage()).toMatchObject({ content: "Partial answer continued", status: "complete" });
  expect(getChat("owner", "chat")?.activeRun).toBeUndefined();
});

it.each(["failed", "completed", "paused"])("restores known %s evidence before the upstream reconnect fails", async status => {
  await prefix("Saved answer", undefined, status === "failed", status === "failed" ? undefined :
    { type: "run_finished", run_id: "root", status });
  const cursor = status === "failed" ? 8 : 9;
  const client = runtime(backend([]));
  vi.mocked(client.fetchResponse).mockRejectedValue(new TypeError("fetch failed"));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(cursor));
  const onText = vi.fn(), onDone = vi.fn(), onPaused = vi.fn();
  const observation = streamExistingChatRun("chat", "root", { onText, onDone, onPaused },
    { nextEventIndex: cursor, assistantMessageId: assistantId });
  if (status === "failed") {
    await expect(observation).rejects.toMatchObject({ code: "canonical_persistence_failed", detail: "Save failed" });
    expect(onDone).not.toHaveBeenCalled();
  } else {
    await expect(observation).resolves.toBe("Saved answer");
    expect(status === "paused" ? onPaused : onDone).toHaveBeenCalledExactlyOnceWith("Saved answer");
  }
  expect(onText).toHaveBeenLastCalledWith(status === "failed" ? "Saved answer\n\nSave failed" : "Saved answer");
});

it.each([
  { name: "empty", text: "", reasoning: "" },
  { name: "partial", text: "Partial answer", reasoning: "" },
  { name: "reasoning only", text: "", reasoning: "Thinking" },
])("does not finalize a first POST at EOF with $name output", async ({ text, reasoning }) => {
  runtime(backend([bootstrap, { type: "text_delta", content: text },
    { type: "reasoning_delta", content: reasoning }, { type: "run_input_queued", run_id: "root", index: 7 }]));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await startPost());
  const onDone = vi.fn();
  await expect(streamChatMessage("chat", payload, { onDone })).rejects
    .toMatchObject({ code: "stream_observation_interrupted" });
  expect(selectedMessage()).toMatchObject({ content: text, status: "streaming" });
  expect(getChat("owner", "chat")?.activeRun).toMatchObject({ runId: "root", nextEventIndex: 8 });
  expect(onDone).not.toHaveBeenCalled();
});

it.each(["completed", "delegated"])("accepts an explicit %s receipt with no visible text", async status => {
  runtime(backend([bootstrap, { type: "run_finished", run_id: "root", status, index: 0 }]));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await startPost());
  const onDone = vi.fn();
  await expect(streamChatMessage("chat", payload, { onDone })).resolves.toBe("");
  expect(onDone).toHaveBeenCalledExactlyOnceWith("");
  expect(selectedMessage().status).toBe("complete");
  expect(getChat("owner", "chat")?.activeRun).toBeUndefined();
});

it("retires a paused receipt on a real resume before an interrupted suffix", async () => {
  await prefix("Partial answer", undefined, false,
    { type: "run_finished", run_id: "root", status: "paused" });
  runtime(backend([bootstrap, { type: "run_resumed", run_id: "root", index: 9 }]));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(9));
  const onDone = vi.fn(), onPaused = vi.fn();
  await expect(streamExistingChatRun("chat", "root", { onDone, onPaused },
    { nextEventIndex: 9, assistantMessageId: assistantId })).rejects
    .toMatchObject({ code: "stream_observation_interrupted" });
  expect(selectedMessage()).toMatchObject({ content: "Partial answer", status: "streaming" });
  expect(getChat("owner", "chat")?.activeRun).toMatchObject({ runId: "root", status: "running", nextEventIndex: 10 });
  expect(onDone).not.toHaveBeenCalled();
  expect(onPaused).not.toHaveBeenCalled();
});

it.each([
  { name: "visible boundary whitespace", raw: "Useful partial answer ", delta: "plus suffix", answer: "Useful partial answer plus suffix" },
  { name: "open thinking block", raw: "Visible<thinking>hidden", delta: " more</thinking> answer", answer: "Visible answer", reasoning: "hidden more" },
  { name: "split thinking tag", raw: "Visible<thi", delta: "nking>hidden</thinking> answer", answer: "Visible answer", reasoning: "hidden" },
  { name: "separate reasoning lane", raw: "Visible", separate: "hidden", delta: " more", answer: "Visible more", reasoning: "hidden" },
  { name: "DSML boundary", raw: "Visible<｜｜DSML｜｜tool_calls>private", delta: "</｜｜DSML｜｜tool_calls> answer", answer: "Visible answer" },
  { name: "cumulative delta", raw: "Useful partial", delta: "Useful partial answer", answer: "Useful partial answer" },
  { name: "full text replacement", raw: "Old answer", delta: " ignored", answer: "Replacement", replacement: { type: "text_done", full_text: "Replacement" } },
  { name: "replacement clears inline reasoning", raw: "Visible<thinking>hidden", delta: " more</thinking> answer", answer: "Replacement", replacement: { type: "text_done", full_text: "Replacement" } },
  { name: "late authoritative replacement", raw: "Old answer", delta: " ignored", answer: "Replacement", late: { type: "turn_complete", assistant_text: "Replacement" } },
])("continues genuine raw state across GET disconnect: $name", async ({ raw, separate, delta, answer, reasoning, replacement, late }) => {
  await prefix(raw, separate);
  const next = backend([bootstrap, { type: "text_delta", content: delta }, ...(replacement ? [replacement] : []),
    failure, ...(late ? [late] : []), terminal,
    { type: "run_finished", run_id: "root", status: "completed" }]);
  const nextRuntime = runtime(next);
  const response = await attach(8);
  vi.spyOn(globalThis, "fetch").mockResolvedValue(response);
  const onText = vi.fn(), onReasoning = vi.fn(), onDone = vi.fn(), onRunStarted = vi.fn();
  await expect(streamExistingChatRun("chat", "root", { onText, onReasoning, onReasoningDone: onReasoning, onDone, onRunStarted },
    { nextEventIndex: 8, assistantMessageId: assistantId })).rejects.toMatchObject({ code: "canonical_persistence_failed", detail: "Save failed" });
  expect(onText).toHaveBeenLastCalledWith(answer);
  expect(onReasoning).toHaveBeenLastCalledWith(reasoning ?? "");
  expect(onDone).not.toHaveBeenCalled();
  expect(onRunStarted).not.toHaveBeenCalled();
  expect(selectedMessage()).toMatchObject({ content: `${answer}\n\nSave failed`, status: "failed", reasoning, reasoningStatus: undefined });
  expect(nextRuntime.fetchResponse).toHaveBeenCalledWith("/chat/runs/root/stream?last_index=8", expect.anything());
  expect(next.releaseLock).toHaveBeenCalled();
  expect(JSON.stringify(getChat("owner", "chat"))).not.toContain("assistantRawText");
});

it("does not append an already saved cause again, and retains a distinct later cause", async () => {
  await prefix("Partial answer", undefined, true);
  runtime(backend([bootstrap, failure, terminal]));
  await (await attach(8)).text();
  expect(selectedMessage().content).toBe("Partial answer\n\nSave failed");
  runtime(backend([bootstrap, { ...failure, error: "Different failure" }, { ...terminal, error: "Different failure" }]));
  await (await attach(8)).text();
  expect(selectedMessage().content).toBe("Partial answer\n\nSave failed\n\nDifferent failure");
});

it("rejects overlapping writers without starting another transport; independent chats still work", async () => {
  const first = backend([bootstrap], true);
  const firstRuntime = runtime(first);
  const response = await startPost();
  const conflict = await attach();
  expect(conflict.status).toBe(409);
  expect(await conflict.json()).toMatchObject({ code: "stream_projection_busy" });
  expect(firstRuntime.fetchResponse).toHaveBeenCalledTimes(1);
  chat("owner", "other");
  runtime(backend([{ ...bootstrap, session_id: "other" }, terminal]));
  const independent = await startPost("other");
  expect(independent.status).toBe(200);
  await independent.text();
  await response.body!.cancel();
});

it.each(["absent", "ahead", "wrong root", "wrong owner"])("fails closed for %s continuation and preserves the message", async (kind) => {
  if (kind !== "absent" && kind !== "wrong owner") await prefix("Partial answer");
  if (kind === "wrong owner") { chat("other"); vi.mocked(requireRuntimeUser).mockResolvedValue({ user: { user_id: "other" }, response: null } as never); }
  const nextRuntime = runtime(backend([bootstrap]));
  if (kind === "wrong root") getStore("owner").chats.find(item => item.id === "chat")!.streamProjection!.state.runId = "other";
  const before = JSON.stringify(getChat(kind === "wrong owner" ? "other" : "owner", "chat"));
  const response = await attach(kind === "ahead" ? 9 : 8);
  expect(response.status).toBe(409);
  expect(await response.json()).toMatchObject({ code: "stream_continuation_unavailable" });
  expect(nextRuntime.fetchResponse).not.toHaveBeenCalled();
  expect(JSON.stringify(getChat(kind === "wrong owner" ? "other" : "owner", "chat"))).toBe(before);
});

it("fences aborted writers and stale releases and retires state when a new message is selected", () => {
  const started = beginStreamingMessage("owner", "chat", { content: "start" })!;
  assistantId = started.assistantMessage.id;
  const oldWriter = new AbortController();
  const old = claimChatStreamProjection("owner", "chat", assistantId, oldWriter, { sessionId: "session" });
  oldWriter.abort();
  const next = claimChatStreamProjection("owner", "chat", assistantId, new AbortController(), { sessionId: "session" });
  const mutate = vi.fn();
  expect(withChatStreamProjection(old, mutate)).toBe(false);
  releaseChatStreamProjection(old);
  expect(withChatStreamProjection(next, mutate)).toBe(true);
  expect(() => beginStreamingMessage("owner", "chat", { content: "next" }))
    .toThrow("already being observed");
  expect(withChatStreamProjection(next, mutate)).toBe(true);
  releaseChatStreamProjection(next);
  const selected = beginStreamingMessage("owner", "chat", { content: "next" })!;
  expect(selected.assistantMessage.id).not.toBe(assistantId);
  expect(withChatStreamProjection(next, mutate)).toBe(false);
});

it("rejects a forged upstream initialization before forwarding or mutating display", async () => {
  await prefix("Partial answer");
  runtime(backend([{ type: "chat_stream_init", state: { assistantRawText: "forged" } }]));
  const wire = await (await attach(8)).text();
  expect(wire).not.toContain("forged");
  expect(wire).toContain("reserved Web initialization");
  expect(selectedMessage().content).toBe("Partial answer");
});

it("initializes an older browser from the current cursor and suppresses indexed replay duplicates", async () => {
  await prefix("Partial answer");
  const nextRuntime = runtime(backend([bootstrap,
    { type: "text_done", full_text: "stale replacement", index: 7 },
    { type: "text_delta", content: " continued" }, failure, terminal]));
  const response = await attach(3);
  vi.spyOn(globalThis, "fetch").mockResolvedValue(response);
  const onText = vi.fn();
  await expect(streamExistingChatRun("chat", "root", { onText }, { nextEventIndex: 3, assistantMessageId: assistantId }))
    .rejects.toMatchObject({ code: "canonical_persistence_failed" });
  expect(onText).toHaveBeenLastCalledWith("Partial answer continued");
  expect(onText.mock.calls.flat()).not.toContain("stale replacement");
  expect(nextRuntime.fetchResponse).toHaveBeenCalledWith("/chat/runs/root/stream?last_index=8", expect.anything());
  expect(selectedMessage().content).toBe("Partial answer continued\n\nSave failed");
});

it("binds POST continuation after creating a distinct backend session", async () => {
  getStore("owner").chats.find(item => item.id === "chat")!.backendSessionId = undefined;
  await prefix("Partial answer");
  expect(getChat("owner", "chat")?.session?.backendSessionId).toBe("session");
  expect(getStore("owner").chats.find(item => item.id === "chat")?.streamProjection?.sessionId).toBe("session");
  runtime(backend([bootstrap, failure, terminal]));
  const response = await attach(8);
  expect(response.status).toBe(200);
  await response.text();
  expect(selectedMessage().content).toBe("Partial answer\n\nSave failed");
});

it.each([undefined, 0])("rejects missing continuation even at cursor %s without replacing saved output", async cursor => {
  selectedMessage().content = "Saved partial answer";
  selectedMessage().reasoning = "Saved reasoning";
  const client = runtime(backend([bootstrap, failure]));
  const response = await attach(cursor);
  expect(response.status).toBe(409);
  expect(await response.json()).toMatchObject({ code: "stream_continuation_unavailable" });
  expect(selectedMessage()).toMatchObject({ content: "Saved partial answer", reasoning: "Saved reasoning" });
  expect(client.fetchResponse).not.toHaveBeenCalled();
});

it.each([null, "999"])("delivers protocol rejection before aborting its writer (header=%s)", async header => {
  const upstream = backend([bootstrap]);
  if (header === null) upstream.headers.delete("x-astra-agent-interaction-api-major");
  else upstream.headers.set("x-astra-agent-interaction-api-major", header);
  runtime(upstream);
  const response = await startPost();
  vi.spyOn(globalThis, "fetch").mockResolvedValue(response);
  const onDone = vi.fn();
  await expect(streamChatMessage("chat", payload, { onDone })).rejects.toMatchObject({ code: "RUNTIME_PROTOCOL_MISMATCH" });
  expect(onDone).not.toHaveBeenCalled();
  expect(selectedMessage().status).toBe("failed");
  expect((await attach(0)).status).toBe(409);
});

it.each([
  { type: "run_waiting", reason: "waiting: tool_approval", status: "waiting" },
  { type: "run_waiting", reason: "waiting: executor_offline", status: "blocked" },
  { type: "run_paused", status: "paused" },
])("restores $status without a synthetic lifecycle start, then accepts a real resume", async ({ status, ...event }) => {
  await prefix("Partial answer", undefined, false, { ...event, run_id: "root" });
  runtime(backend([bootstrap]));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(9));
  const onRunStarted = vi.fn(), onRunUpdated = vi.fn(), onPaused = vi.fn(), onDone = vi.fn();
  await expect(streamExistingChatRun("chat", "root", { onRunStarted, onRunUpdated, onPaused, onDone },
    { nextEventIndex: 9, assistantMessageId: assistantId })).resolves.toBe("Partial answer");
  expect(onRunStarted).not.toHaveBeenCalled();
  expect(onRunUpdated).not.toHaveBeenCalled();
  expect(onPaused).toHaveBeenCalledWith("Partial answer");
  expect(onDone).not.toHaveBeenCalled();
  expect(getChat("owner", "chat")?.activeRun?.status).toBe(status);
  runtime(backend([bootstrap, { type: "run_resumed", run_id: "root", index: 9 },
    { type: "text_delta", content: " continued" }, { type: "run_finished", run_id: "root", status: "completed", index: 10 }]));
  vi.mocked(globalThis.fetch).mockResolvedValue(await attach(9));
  await expect(streamExistingChatRun("chat", "root", { onRunStarted, onRunUpdated, onDone },
    { nextEventIndex: 9, assistantMessageId: assistantId })).resolves.toBe("Partial answer continued");
  expect(onRunStarted).not.toHaveBeenCalled();
  expect(onRunUpdated).toHaveBeenCalledWith(expect.objectContaining({ status: "running" }));
  expect(onDone).toHaveBeenCalledWith("Partial answer continued");
});

function queuedRuntime(events: Array<Record<string, unknown>> = [], pending = false, eventsCount: number | undefined = 8) {
  const client = runtime(backend(events, pending));
  vi.spyOn(client.sdk, "getRunStatus").mockResolvedValue({ runId: "root", sessionId: "session", status: "running", eventsCount } as never);
  vi.spyOn(client.sdk, "submitRunInput").mockResolvedValue({} as never);
  return client;
}

it("keeps accepted queued input idempotent after its projection has received text", async () => {
  await prefix("Previous partial answer");
  const client = queuedRuntime([bootstrap, { type: "text_delta", content: "Next answer" }], true);
  const queued = await queueDeferredRunInput("owner", "chat", { content: "follow up", pendingMessageId: "queued-user" });
  assistantId = queued!.assistantMessage.id;
  const response = await attach(8);
  expect(response.status).toBe(200);
  const reader = response.body!.getReader();
  await reader.read();
  await vi.waitFor(() => expect(selectedMessage().content).toBe("Next answer"));
  const projection = getStore("owner").chats.find(item => item.id === "chat")!.streamProjection!;
  const writer = projection.writer;
  const retry = await queueDeferredRunInput("owner", "chat", { content: "follow up", pendingMessageId: "queued-user" });
  expect(retry!.assistantMessage.id).toBe(assistantId);
  expect(getStore("owner").chats.find(item => item.id === "chat")!.streamProjection).toBe(projection);
  expect(projection.state.assistantRawText).toBe("Next answer");
  expect(projection.writer).toBe(writer);
  expect(writer!.signal.aborted).toBe(false);
  expect(client.sdk.submitRunInput).toHaveBeenCalledOnce();
  await reader.cancel();
  runtime(backend([bootstrap, failure, terminal]));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(8));
  await expect(streamExistingChatRun("chat", "root", {}, { nextEventIndex: 8, assistantMessageId: assistantId }))
    .rejects.toMatchObject({ code: "canonical_persistence_failed" });
  expect(selectedMessage().content).toBe("Next answer\n\nSave failed");
});

it("converges concurrent retries on one selected assistant without resetting its writer", async () => {
  await prefix("Previous answer");
  const client = queuedRuntime([bootstrap, { type: "text_delta", content: "Next answer" }], true);
  const replies: Array<() => void> = [];
  vi.mocked(client.sdk.submitRunInput).mockImplementation(() => new Promise(resolve => { replies.push(() => resolve({} as never)); }));
  const first = queueDeferredRunInput("owner", "chat", { content: "follow up", pendingMessageId: "same-action" });
  const second = queueDeferredRunInput("owner", "chat", { content: "follow up", pendingMessageId: "same-action" });
  await vi.waitFor(() => expect(replies).toHaveLength(2));
  replies[0]();
  const accepted = await first;
  assistantId = accepted!.assistantMessage.id;
  const reader = (await attach(8)).body!.getReader();
  await reader.read();
  await vi.waitFor(() => expect(selectedMessage().content).toBe("Next answer"));
  const projection = getStore("owner").chats.find(item => item.id === "chat")!.streamProjection!;
  replies[1]();
  expect((await second)!.assistantMessage.id).toBe(assistantId);
  expect(getStore("owner").chats.find(item => item.id === "chat")!.streamProjection).toBe(projection);
  expect(projection.state.assistantRawText).toBe("Next answer");
  expect(projection.writer!.signal.aborted).toBe(false);
  expect(getChat("owner", "chat")!.messages.filter(message => message.id === "same-action")).toHaveLength(1);
  expect(new Set(vi.mocked(client.sdk.submitRunInput).mock.calls.map(([, request]) => request.idempotencyKey)).size).toBe(1);
  await reader.cancel();
});

it.each([8, undefined])("creates a missing queued assistant only from a validated cursor (%s)", async cursor => {
  await prefix("Previous answer");
  const record = getStore("owner").chats.find(item => item.id === "chat")!;
  const previous = record.streamProjection;
  record.messages.push({ id: "accepted-user", role: "user", content: "already accepted", createdAt: "2026-10-21T00:00:01Z" });
  const client = queuedRuntime([], false, cursor);
  if (cursor === undefined) {
    // Omit the SDK field deliberately; this owner must not invent a zero cursor.
    vi.mocked(client.sdk.getRunStatus).mockResolvedValue({ runId: "root", sessionId: "session", status: "running" } as never);
    await expect(queueDeferredRunInput("owner", "chat", { content: "already accepted", pendingMessageId: "accepted-user" }))
      .rejects.toThrow("valid continuation cursor");
    expect(record.streamProjection).toBe(previous);
  } else {
    const result = await queueDeferredRunInput("owner", "chat", { content: "already accepted", pendingMessageId: "accepted-user" });
    expect(record.streamProjection?.assistantMessageId).toBe(result!.assistantMessage.id);
    expect(record.streamProjection?.state).toMatchObject({ assistantRawText: "", nextEventIndex: cursor });
  }
  expect(client.sdk.submitRunInput).not.toHaveBeenCalled();
});

it.each(["rejected", "client", "status", "submission", "session"])("retains the projection when queued submission is %s", async point => {
  await prefix("Previous answer");
  const record = getStore("owner").chats.find(item => item.id === "chat")!;
  const previous = record.streamProjection;
  const count = record.messages.length;
  const client = queuedRuntime();
  const changeRun = () => { record.activeRun = { ...record.activeRun!, runId: "replacement" }; };
  if (point === "rejected") vi.mocked(client.sdk.submitRunInput).mockRejectedValue(new Error("submission rejected"));
  if (point === "client") vi.mocked(requireStoreRuntimeClient).mockImplementation(async () => { changeRun(); return client; });
  if (point === "status") vi.mocked(client.sdk.getRunStatus).mockImplementation(async () => { changeRun(); return { runId: "root", sessionId: "session", status: "running", eventsCount: 8 } as never; });
  if (point === "session") vi.mocked(client.sdk.submitRunInput).mockImplementation(async () => { record.backendSessionId = "replacement"; return {} as never; });
  if (point === "submission") vi.mocked(client.sdk.submitRunInput).mockImplementation(async () => { changeRun(); return {} as never; });
  await expect(queueDeferredRunInput("owner", "chat", { content: "follow up", pendingMessageId: "new-action" })).rejects.toThrow();
  expect(record.streamProjection).toBe(previous);
  expect(record.messages).toHaveLength(count);
  if (point === "client" || point === "status") expect(client.sdk.submitRunInput).not.toHaveBeenCalled();
});

it("does not reselect an older queued assistant when its action is retried", async () => {
  await prefix("Previous answer");
  const client = queuedRuntime();
  const first = await queueDeferredRunInput("owner", "chat", { content: "first", pendingMessageId: "first-action" });
  const second = await queueDeferredRunInput("owner", "chat", { content: "second", pendingMessageId: "second-action" });
  const record = getStore("owner").chats.find(item => item.id === "chat")!;
  const current = record.streamProjection;
  const retry = await queueDeferredRunInput("owner", "chat", { content: "first", pendingMessageId: "first-action" });
  expect(retry!.assistantMessage.id).toBe(first!.assistantMessage.id);
  expect(retry!.activeRun?.assistantMessageId).toBe(second!.assistantMessage.id);
  expect(record.streamProjection).toBe(current);
  expect(client.sdk.submitRunInput).toHaveBeenCalledTimes(2);
});

it("retires only the old observer and fences its late release when queued input selects a new message", async () => {
  const oldBackend = backend([bootstrap, { type: "text_delta", content: "Old answer" },
    { type: "run_input_queued", run_id: "root", index: 7 }], true);
  const oldClient = runtime(oldBackend);
  const oldReader = (await startPost()).body!.getReader();
  await oldReader.read();
  await vi.waitFor(() => expect(selectedMessage().content).toBe("Old answer"));
  const oldWriter = getStore("owner").chats.find(item => item.id === "chat")!.streamProjection!.writer!;
  const client = queuedRuntime([bootstrap, { type: "text_delta", content: "New answer" }], true);
  const cancelOldRun = vi.spyOn(oldClient.sdk, "cancelRun");
  const cancelNewRun = vi.spyOn(client.sdk, "cancelRun");
  const result = await queueDeferredRunInput("owner", "chat", { content: "follow up", pendingMessageId: "next-action" });
  assistantId = result!.assistantMessage.id;
  expect(oldWriter.signal.aborted).toBe(true);
  const reader = (await attach(8)).body!.getReader();
  await reader.read();
  await vi.waitFor(() => expect(selectedMessage().content).toBe("New answer"));
  const projection = getStore("owner").chats.find(item => item.id === "chat")!.streamProjection!;
  await oldReader.cancel();
  await vi.waitFor(() => expect(oldBackend.releaseLock).toHaveBeenCalled());
  expect(getStore("owner").chats.find(item => item.id === "chat")!.streamProjection).toBe(projection);
  expect(projection.writer!.signal.aborted).toBe(false);
  expect(cancelOldRun).not.toHaveBeenCalled();
  expect(cancelNewRun).not.toHaveBeenCalled();
  await reader.cancel();
});


it("resumes the same completed paused assistant through GET and the public consumer", async () => {
  getStore("owner").chats[0].messages = [];
  await prefix("Paused partial answer", "Saved reasoning", false,
    { type: "run_finished", run_id: "root", status: "paused" });
  const count = getChat("owner", "chat")!.messages.length;
  expect(selectedMessage()).toMatchObject({ status: "complete" });
  const client = runtime(backend([bootstrap,
    { type: "run_resumed", run_id: "root", index: 9 },
    { type: "text_delta", content: " continued", index: 10 },
    { type: "run_finished", run_id: "root", status: "completed", index: 11 }]));
  vi.spyOn(client.sdk, "resumeRun").mockResolvedValue({ status: "running" });
  const resumed = await resumeActiveRun("owner", "chat");
  expect(resumed?.activeRun).toMatchObject({ assistantMessageId: assistantId, nextEventIndex: 9 });
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(resumed!.activeRun!.nextEventIndex!));
  const onReasoning = vi.fn();
  await expect(streamExistingChatRun("chat", "root", { onReasoning }, {
    assistantMessageId: resumed!.activeRun!.assistantMessageId!, nextEventIndex: resumed!.activeRun!.nextEventIndex!,
  })).resolves.toBe("Paused partial answer continued");
  expect(onReasoning).toHaveBeenCalledWith("Saved reasoning");
  expect(getChat("owner", "chat")!.messages).toHaveLength(count);
  expect(selectedMessage()).toMatchObject({ content: "Paused partial answer continued", reasoning: "Saved reasoning", status: "complete" });
});

it.each(["fetch failure", "EOF"])("retires the paused receipt after resume succeeds before %s", async transport => {
  await prefix("Paused answer", "Saved reasoning", false,
    { type: "run_finished", run_id: "root", status: "paused" });
  const client = runtime(backend([bootstrap]));
  vi.spyOn(client.sdk, "resumeRun").mockResolvedValue({ status: "running" });
  if (transport === "fetch failure") vi.mocked(client.fetchResponse).mockRejectedValue(new TypeError("fetch failed"));
  await expect(resumeActiveRun("owner", "chat")).resolves.toMatchObject({ activeRun: { status: "running" } });
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(9));
  const onDone = vi.fn(), onPaused = vi.fn();
  await expect(streamExistingChatRun("chat", "root", { onDone, onPaused },
    { nextEventIndex: 9, assistantMessageId: assistantId })).rejects
    .toMatchObject({ code: "stream_observation_interrupted" });
  expect(onDone).not.toHaveBeenCalled();
  expect(onPaused).not.toHaveBeenCalled();
  expect(selectedMessage()).toMatchObject({ content: "Paused answer", reasoning: "Saved reasoning", status: "streaming" });
  expect(getChat("owner", "chat")?.activeRun).toMatchObject({ runId: "root", status: "running", nextEventIndex: 9 });
});

it("keeps an upstream run 404 separate from a missing Web chat", async () => {
  await prefix("Partial answer");
  const client = runtime(backend([]));
  vi.mocked(client.fetchResponse).mockResolvedValue(new Response("Run unavailable", { status: 404 }));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(8));
  const error = await streamExistingChatRun("chat", "root", {},
    { nextEventIndex: 8, assistantMessageId: assistantId }).catch(error => error);
  expect(isNotFoundError(error)).toBe(false);
  expect(error).toMatchObject({ code: "stream_observation_interrupted" });
  expect(selectedMessage()).toMatchObject({ content: "Partial answer", status: "streaming" });
});

it.each(["waiting", "child", "canonical failure"])("orders resume acknowledgement against %s observations", async fact => {
  await prefix("Paused answer", undefined, false,
    { type: "run_finished", run_id: "root", status: "paused" });
  const events = fact === "child"
    ? [{ type: "run_finished", run_id: "child", status: "completed", index: 9 }]
    : fact === "canonical failure" ? [failure]
    : [{ type: "run_resumed", run_id: "root", index: 9 },
      { type: "run_waiting", run_id: "root", reason: "waiting: tool_approval", index: 10 }];
  const client = runtime(backend([bootstrap, ...events]));
  let acknowledge!: (value: { status: string }) => void;
  const resume = vi.spyOn(client.sdk, "resumeRun").mockImplementation(() => new Promise(resolve => { acknowledge = resolve; }));
  const pending = resumeActiveRun("owner", "chat");
  await vi.waitFor(() => expect(resume).toHaveBeenCalledOnce());
  await (await attach(9)).text();
  acknowledge({ status: "running" });
  if (fact === "canonical failure") {
    await expect(pending).rejects.toThrow("run changed");
    expect(getChat("owner", "chat")?.activeRun).toBeUndefined();
    expect(selectedMessage()).toMatchObject({ content: "Paused answer\n\nSave failed", status: "failed" });
  } else {
    await expect(pending).resolves.toMatchObject({ activeRun: { runId: "root",
      status: fact === "child" ? "running" : "waiting", nextEventIndex: fact === "child" ? 10 : 11 } });
    expect(selectedMessage()).toMatchObject({ content: "Paused answer", status: "streaming" });
  }
});

it("keeps the paused receipt when resume is rejected", async () => {
  await prefix("Paused answer", undefined, false,
    { type: "run_finished", run_id: "root", status: "paused" });
  const client = runtime(backend([]));
  vi.spyOn(client.sdk, "resumeRun").mockRejectedValue(new Error("Resume rejected"));
  await expect(resumeActiveRun("owner", "chat")).rejects.toThrow("Resume rejected");
  vi.mocked(client.fetchResponse).mockRejectedValue(new TypeError("fetch failed"));
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(9));
  const onPaused = vi.fn();
  await expect(streamExistingChatRun("chat", "root", { onPaused },
    { nextEventIndex: 9, assistantMessageId: assistantId })).resolves.toBe("Paused answer");
  expect(onPaused).toHaveBeenCalledExactlyOnceWith("Paused answer");
  expect(getChat("owner", "chat")?.activeRun?.status).toBe("paused");
});

it("hydrates status without advancing the genuine projection cursor", async () => {
  await prefix("Projected partial answer", undefined, false, { type: "run_resumed", run_id: "root" });
  const client = runtime(backend([bootstrap,
    { type: "text_delta", content: " continued", index: 9 },
    { type: "run_waiting", run_id: "root", reason: "waiting: tool_approval", index: 11 }]));
  vi.mocked(getStoreRuntimeClient).mockResolvedValue(client);
  vi.spyOn(client.sdk, "getSessionConversation").mockResolvedValue({ kind: "conversation",
    session: { session_id: "session", status: "active", metadata: { source: "web_v1" }, created_at: "2026-10-21T00:00:00Z" },
    selected_root: { run_id: "root", status: "waiting", waiting_for: "tool_approval", created_at: "2026-10-21T00:00:00Z" },
    items: [], history_loaded: true, raw_page: { next_before_seq: null, has_more: false }, truncated: false,
  });
  const detail = await getChatHydrated("owner", "chat");
  expect(detail?.activeRun).toMatchObject({ status: "waiting", nextEventIndex: 9, assistantMessageId: assistantId });
  vi.spyOn(globalThis, "fetch").mockResolvedValue(await attach(detail!.activeRun!.nextEventIndex!));
  await expect(streamExistingChatRun("chat", "root", {}, {
    assistantMessageId: detail!.activeRun!.assistantMessageId!, nextEventIndex: detail!.activeRun!.nextEventIndex!,
  })).resolves.toBe("Projected partial answer continued");
  expect(client.fetchResponse).toHaveBeenCalledWith("/chat/runs/root/stream?last_index=9", expect.anything());
  expect(selectedMessage().content).toBe("Projected partial answer continued");
});

it.each(["root", "session", "message", "protocol"])("does not publish a continuation from a mismatched %s", async mismatch => {
  await prefix("Partial answer");
  const record = getStore("owner").chats[0];
  const projection = record.streamProjection!;
  if (mismatch === "root") projection.state.runId = "other-root";
  if (mismatch === "session") projection.sessionId = "other-session";
  if (mismatch === "message") projection.assistantMessageId = "other-message";
  if (mismatch === "protocol") projection.state.protocolError = true;
  expect(getChat("owner", "chat")?.activeRun).not.toHaveProperty("nextEventIndex");
});

it("rejects an explicit unknown assistant rather than substituting the active assistant", async () => {
  await prefix("Partial answer");
  const client = runtime(backend([bootstrap]));
  const nextUrl = new URL("http://web.test/api/chats/chat/stream?runId=root&assistantMessageId=unknown&last_index=8");
  const response = await GET({ nextUrl } as never, { params: Promise.resolve({ chatId: "chat" }) });
  expect(response.status).toBe(409);
  expect(await response.json()).toMatchObject({ code: "stream_continuation_unavailable" });
  expect(client.fetchResponse).not.toHaveBeenCalled();
  expect(selectedMessage().content).toBe("Partial answer");
});

it.each(["client-root", "ack-root", "ack-session", "ack-message"])("fences resume when its binding changes at %s", async point => {
  await prefix("Paused answer", undefined, false, { type: "run_finished", run_id: "root", status: "paused" });
  const record = getStore("owner").chats[0];
  const client = runtime(backend([]));
  const change = () => {
    if (point.endsWith("root")) record.activeRun = { ...record.activeRun!, runId: "replacement", status: "waiting" };
    if (point.endsWith("session")) record.backendSessionId = "replacement-session";
    if (point.endsWith("message")) record.activeRun = { ...record.activeRun!, assistantMessageId: "replacement-message" };
  };
  const resume = vi.spyOn(client.sdk, "resumeRun").mockImplementation(async () => { change(); return { status: "running" }; });
  if (point === "client-root") vi.mocked(requireStoreRuntimeClient).mockImplementationOnce(async () => { change(); return client; });
  await expect(resumeActiveRun("owner", "chat")).rejects.toThrow("run changed");
  if (point === "client-root") expect(resume).not.toHaveBeenCalled();
  expect(selectedMessage().content).toBe("Paused answer");
  expect(record.activeRun?.status).not.toBe("running");
});


it.each([true, false])("resolves an omitted message only from the active owner (bound=%s)", async bound => {
  await prefix("Partial answer");
  if (!bound) getStore("owner").chats[0].activeRun = undefined;
  runtime(backend([bootstrap, { type: "run_finished", run_id: "root", status: "completed", index: 8 }]));
  const nextUrl = new URL("http://web.test/api/chats/chat/stream?runId=root&last_index=8");
  const response = await GET({ nextUrl } as never, { params: Promise.resolve({ chatId: "chat" }) });
  expect(response.status).toBe(bound ? 200 : 409);
  await response.text();
  expect(selectedMessage().content).toBe("Partial answer");
});


it.each([false, true])("revalidates Stop after runtime admission (replaced=%s)", async replaced => {
  await prefix("Original answer A");
  const binding = { runId: "root", sessionId: "session", assistantMessageId: assistantId };
  const client = runtime(backend([{ ...bootstrap, run_id: "new-root" }]));
  const cancel = vi.spyOn(client.sdk, "cancelRun").mockResolvedValue({} as never);
  let admit!: (value: WebRuntimeClient) => void;
  vi.mocked(requireStoreRuntimeClient).mockImplementationOnce(() => new Promise(resolve => { admit = resolve; }));
  const stopping = stopActiveRun("owner", "chat", binding, { skipSync: true });
  // Attach the rejection handler before completing asynchronous admission.
  const settled = stopping.then(value => ({ value }), error => ({ error }));
  if (replaced) await (await startPost()).text();
  admit(client);
  const result = await settled;
  if (replaced) {
    expect(result).toMatchObject({ error: { message: expect.stringContaining("selected run changed") } });
    expect(cancel).not.toHaveBeenCalled();
  } else {
    expect(result).toHaveProperty("value");
    expect(cancel).toHaveBeenCalledExactlyOnceWith("root");
  }
  expect(getChat("owner", "chat")?.activeRun?.runId).toBe(replaced ? "new-root" : "root");
});

it("rejects a stale Stop request before it can cancel the successor", async () => {
  await prefix("Original answer A");
  const oldAssistantId = assistantId;
  const client = runtime(backend([{ ...bootstrap, run_id: "new-root" }]));
  const cancel = vi.spyOn(client.sdk, "cancelRun").mockResolvedValue({} as never);
  await (await startPost()).text();
  expect(getChat("owner", "chat")?.activeRun?.runId).toBe("new-root");
  const { POST: stop } = await import("@/app/api/chats/[chatId]/stop/route");
  const response = await stop(new Request("http://web.test/api/chats/chat/stop", {
    method: "POST", body: JSON.stringify({ runId: "root", sessionId: "session", assistantMessageId: oldAssistantId }),
  }), { params: Promise.resolve({ chatId: "chat" }) });
  expect(cancel).not.toHaveBeenCalled();
  expect(response.status).toBe(409);
  expect(getChat("owner", "chat")?.activeRun?.runId).toBe("new-root");
});


it.each(["session", "root", "message"])("rejects a stale Stop %s before runtime admission", async changed => {
  await prefix("Saved answer");
  vi.mocked(requireStoreRuntimeClient).mockClear();
  const binding = { runId: "root", sessionId: "session", assistantMessageId: assistantId };
  if (changed === "session") binding.sessionId = "other-session";
  if (changed === "root") binding.runId = "other-root";
  if (changed === "message") binding.assistantMessageId = "other-assistant";
  await expect(stopActiveRun("owner", "chat", binding, { skipSync: true }))
    .rejects.toThrow("selected run changed");
  expect(requireStoreRuntimeClient).not.toHaveBeenCalled();
  expect(selectedMessage().content).toBe("Saved answer");
});

it.each(["completed", "rejected"])("fences late Stop %s after dispatch and replacement", async outcome => {
  await prefix("Original answer A");
  const binding = { runId: "root", sessionId: "session", assistantMessageId: assistantId };
  const client = runtime(backend([{ ...bootstrap, run_id: "new-root" }]));
  let settle!: () => void;
  const cancel = vi.spyOn(client.sdk, "cancelRun").mockImplementation(() => new Promise((resolve, reject) => {
    settle = () => outcome === "completed" ? resolve({} as never) : reject(new Error("cancel rejected"));
  }));
  const result = await stopActiveRun("owner", "chat", binding, { skipSync: true, cancelTimeoutMs: 1 });
  expect(result?.cancelPending).toBe(true);
  await (await startPost()).text();
  const successor = getChat("owner", "chat");
  settle();
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(cancel).toHaveBeenCalledExactlyOnceWith("root");
  expect(getChat("owner", "chat")).toEqual(successor);
  expect(getChat("owner", "chat")?.activeRun?.runId).toBe("new-root");
});

it("fences a Stop status read that completes after a new public POST", async () => {
  await prefix("Original answer A");
  const binding = { runId: "root", sessionId: "session", assistantMessageId: assistantId };
  const client = runtime(backend([{ ...bootstrap, run_id: "new-root" }]));
  vi.spyOn(client.sdk, "cancelRun").mockResolvedValue({} as never);
  let settle!: (value: unknown) => void;
  const status = vi.spyOn(client.sdk, "getRunStatus").mockImplementation(() => new Promise(resolve => { settle = resolve as never; }));
  vi.mocked(getStoreRuntimeClient).mockResolvedValue(client);
  const stopping = stopActiveRun("owner", "chat", binding, { skipSync: true });
  await vi.waitFor(() => expect(status).toHaveBeenCalledExactlyOnceWith("root"));
  await (await startPost()).text();
  const successor = getChat("owner", "chat");
  settle({ runId: "root", sessionId: "session", status: "cancelled" });
  await stopping;
  expect(getChat("owner", "chat")).toEqual(successor);
});

it("cancels a runtime-only root without stopping an unrelated assistant", async () => {
  await prefix("Unrelated assistant output");
  const record = getStore("owner").chats.find(item => item.id === "chat")!;
  record.activeRun!.assistantMessageId = undefined;
  const client = runtime(backend([]));
  const cancel = vi.spyOn(client.sdk, "cancelRun").mockResolvedValue({} as never);
  const before = { ...record.messages.at(-1)! };
  await stopActiveRun("owner", "chat", { runId: "root", sessionId: "session", assistantMessageId: null }, { skipSync: true });
  expect(cancel).toHaveBeenCalledExactlyOnceWith("root");
  expect(record.messages.at(-1)).toEqual(before);
});


it.each(["output", "root facts"])("preserves newer %s on late Stop rejection", async progress => {
  await prefix("Original answer");
  const binding = { runId: "root", sessionId: "session", assistantMessageId: assistantId };
  const client = runtime(backend([]));
  let reject!: (error: Error) => void;
  vi.spyOn(client.sdk, "cancelRun").mockImplementation(() => new Promise((_resolve, failure) => { reject = failure; }));
  await stopActiveRun("owner", "chat", binding, { skipSync: true, cancelTimeoutMs: 1 });
  const record = getStore("owner").chats.find(item => item.id === "chat")!;
  record.streamProjection!.state.nextEventIndex = 12;
  if (progress === "output") {
    updateStreamingAssistantMessage("owner", "chat", assistantId, {
      content: "New answer", artifacts: [{ id: "new-artifact", kind: "file", title: "New attachment" }],
    });
  } else {
    setChatActiveRun("owner", "chat", { ...record.activeRun!, status: "waiting", waitingFor: "user_input", nextEventIndex: 12 });
  }
  reject(new Error("cancel rejected"));
  await new Promise(resolve => setTimeout(resolve, 0));
  const current = getChat("owner", "chat")!;
  expect(current.activeRun?.nextEventIndex).toBe(12);
  if (progress === "output") {
    expect(current.messages.at(-1)).toMatchObject({ content: "New answer", artifacts: [{ id: "new-artifact" }] });
    expect(current.activeRun?.status).toBe("input-queued");
  } else {
    expect(current.activeRun).toMatchObject({ status: "waiting", waitingFor: "user_input" });
  }
});

it.each(["cancellation_requested", "unavailable", "cancelled"])("returns the retained cancellation lifecycle after a fast Stop ACK (%s)", async statusValue => {
  await prefix("Saved answer");
  const client = runtime(backend([]));
  const cancel = vi.spyOn(client.sdk, "cancelRun").mockResolvedValue(undefined);
  const status = vi.spyOn(client.sdk, "getRunStatus");
  if (statusValue === "unavailable") status.mockRejectedValue(new Error("status unavailable"));
  else status.mockResolvedValue({ runId: "root", sessionId: "session", status: statusValue } as never);
  vi.mocked(getStoreRuntimeClient).mockResolvedValue(client);
  const { POST: stop } = await import("@/app/api/chats/[chatId]/stop/route");
  const response = await stop(new Request("http://web.test/api/chats/chat/stop", {
    method: "POST", body: JSON.stringify({ sessionId: "session", runId: "root", assistantMessageId: assistantId }),
  }), { params: Promise.resolve({ chatId: "chat" }) });
  expect(response.status).toBe(200);
  const result = await response.json();
  expect(result.cancelPending).toBe(false);
  if (statusValue === "cancelled") expect(result.activeRun).toBeUndefined();
  else expect(result.activeRun).toMatchObject({ runId: "root", status: "cancelling", assistantMessageId: assistantId, nextEventIndex: 8 });
  expect(cancel).toHaveBeenCalledExactlyOnceWith("root");
  expect(status).toHaveBeenCalledExactlyOnceWith("root");
});

// @vitest-environment node
vi.mock("@/lib/api/auth-guard", () => ({ requireRuntimeUser: vi.fn() }));
vi.mock("@/lib/runtime-client/server", () => ({
  WebRuntimeClient: class {}, getRuntimeClient: vi.fn(), requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getRuntimeClient, requireRuntimeClient } from "@/lib/runtime-client/server";
import { getChat, getChatHydrated, getStore, queueDeferredRunInput, updateStreamingAssistantMessage } from "@/lib/api/web-store";
import { GET } from "@/app/api/chats/[chatId]/stream/route";

function setup() {
  globalThis.__astraWebStores = undefined;
  vi.mocked(requireRuntimeUser).mockResolvedValue({ user: { user_id: "owner", display_name: null }, response: null } as never);
  const sdk = {
    listRuntimeSessions: vi.fn().mockResolvedValue({ sessions: [{ session_id: "session", status: "active", metadata: { source: "web_v1" } }] }),
    listRuns: vi.fn().mockResolvedValue({ runs: [{ runId: "run", sessionId: "session", parentRunId: null, depth: 0, status: "running" }] }),
    getSessionTranscript: vi.fn().mockResolvedValue({ items: [] }),
    getRunProjection: vi.fn().mockResolvedValue({ run_id: "run", session_id: "session", status: "running",
      opening_input: { original_input: "original request", source_event_id: "input-source" } }),
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
  expect(runtime.sdk.getRunProjection).toHaveBeenCalledTimes(1);
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
  expect(runtime.sdk.getRunProjection).not.toHaveBeenCalled();
});

/**
 * @jest-environment node
 */

jest.mock("@/lib/api/auth-guard", () => ({
  requireRuntimeUser: jest.fn(),
}));

jest.mock("@/lib/api/web-store", () => ({
  beginStreamingMessage: jest.fn(),
  getChat: jest.fn(),
  resolveBackendModelName: jest.fn(),
  setChatActiveRun: jest.fn(),
  updateStreamingAssistantMessage: jest.fn(),
}));

jest.mock("@/lib/runtime-client", () => ({
  WebRuntimeClient: class WebRuntimeClient {},
  readRuntimeErrorDetail: jest.fn(),
  requireRuntimeClient: jest.fn(),
}));

import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  beginStreamingMessage,
  getChat,
  resolveBackendModelName,
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";

const mockRequireRuntimeUser = requireRuntimeUser as jest.MockedFunction<
  typeof requireRuntimeUser
>;
const mockGetChat = getChat as jest.MockedFunction<typeof getChat>;
const mockResolveBackendModelName =
  resolveBackendModelName as jest.MockedFunction<
    typeof resolveBackendModelName
  >;
const mockBeginStreamingMessage = beginStreamingMessage as jest.MockedFunction<
  typeof beginStreamingMessage
>;
const mockSetChatActiveRun = setChatActiveRun as jest.MockedFunction<
  typeof setChatActiveRun
>;
const mockUpdateStreamingAssistantMessage =
  updateStreamingAssistantMessage as jest.MockedFunction<
    typeof updateStreamingAssistantMessage
  >;
const mockRequireRuntimeClient = requireRuntimeClient as jest.MockedFunction<
  typeof requireRuntimeClient
>;

function makeBackendStream() {
  let releasePendingRead: (() => void) | null = null;
  const cancel = jest.fn(() => {
    releasePendingRead?.();
    return Promise.resolve();
  });
  const releaseLock = jest.fn();
  const read = jest.fn(
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
  const releaseLock = jest.fn();
  const cancel = jest.fn();

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

describe("chat stream route proxy cancellation", () => {
  beforeEach(() => {
    jest.clearAllMocks();
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
    mockResolveBackendModelName.mockResolvedValue("backend-model");
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
        getRuntimeSession: jest.fn().mockResolvedValue({}),
        listSessionArtifacts: jest.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: jest.fn().mockResolvedValue({
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
        getRuntimeSession: jest.fn().mockResolvedValue({}),
        listSessionArtifacts: jest.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: jest.fn().mockResolvedValue({
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
});

describe("chat stream route artifact fetch optimization", () => {
  beforeEach(() => {
    jest.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: "user-a" },
      response: null,
    } as never);
    mockResolveBackendModelName.mockResolvedValue("backend-model");
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
    const listSessionArtifacts = jest.fn().mockResolvedValue({ artifacts: [] });
    const runtime = {
      sdk: {
        getRuntimeSession: jest.fn().mockResolvedValue({}),
        listSessionArtifacts,
      },
      fetchResponse: jest.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    await POST(
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
    const listSessionArtifacts = jest.fn().mockResolvedValue({ artifacts: [] });
    const runtime = {
      sdk: {
        getRuntimeSession: jest.fn().mockResolvedValue({}),
        listSessionArtifacts,
      },
      fetchResponse: jest.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    await POST(
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

    expect(listSessionArtifacts).toHaveBeenCalledTimes(1);
  });

  it("calls getRuntimeSession and resolveBackendModelName in parallel", async () => {
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
    const getRuntimeSession = jest.fn().mockImplementation(async () => {
      callOrder.push("session-start");
      // Small delay to simulate network
      await new Promise((r) => setTimeout(r, 10));
      callOrder.push("session-end");
      return {};
    });

    // Override resolveBackendModelName to track timing
    mockResolveBackendModelName.mockImplementation(async () => {
      callOrder.push("model-start");
      await new Promise((r) => setTimeout(r, 10));
      callOrder.push("model-end");
      return "backend-model";
    });

    const runtime = {
      sdk: {
        getRuntimeSession,
        listSessionArtifacts: jest.fn().mockResolvedValue({ artifacts: [] }),
      },
      fetchResponse: jest.fn().mockResolvedValue({
        ok: true,
        body: backend.body,
      }),
    };
    mockRequireRuntimeClient.mockResolvedValue(runtime as never);

    await POST(
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

    // Both calls should have been made
    expect(getRuntimeSession).toHaveBeenCalledTimes(1);
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

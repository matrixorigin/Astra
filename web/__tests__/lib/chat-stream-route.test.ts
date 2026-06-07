/**
 * @jest-environment node
 */

jest.mock('@/lib/api/auth-guard', () => ({
  requireRuntimeUser: jest.fn(),
}));

jest.mock('@/lib/api/web-store', () => ({
  beginStreamingMessage: jest.fn(),
  getChatHydrated: jest.fn(),
  resolveBackendModelName: jest.fn(),
  setChatActiveRun: jest.fn(),
  updateStreamingAssistantMessage: jest.fn(),
}));

jest.mock('@/lib/runtime-client', () => ({
  WebRuntimeClient: class WebRuntimeClient {},
  readRuntimeErrorDetail: jest.fn(),
  requireRuntimeClient: jest.fn(),
}));

import { requireRuntimeUser } from '@/lib/api/auth-guard';
import {
  beginStreamingMessage,
  getChatHydrated,
  resolveBackendModelName,
  updateStreamingAssistantMessage,
} from '@/lib/api/web-store';
import { requireRuntimeClient } from '@/lib/runtime-client';

const mockRequireRuntimeUser = requireRuntimeUser as jest.MockedFunction<typeof requireRuntimeUser>;
const mockGetChatHydrated = getChatHydrated as jest.MockedFunction<typeof getChatHydrated>;
const mockResolveBackendModelName = resolveBackendModelName as jest.MockedFunction<typeof resolveBackendModelName>;
const mockBeginStreamingMessage = beginStreamingMessage as jest.MockedFunction<typeof beginStreamingMessage>;
const mockUpdateStreamingAssistantMessage = updateStreamingAssistantMessage as jest.MockedFunction<typeof updateStreamingAssistantMessage>;
const mockRequireRuntimeClient = requireRuntimeClient as jest.MockedFunction<typeof requireRuntimeClient>;

function makeBackendStream() {
  let releasePendingRead: (() => void) | null = null;
  const cancel = jest.fn(() => {
    releasePendingRead?.();
    return Promise.resolve();
  });
  const releaseLock = jest.fn();
  const read = jest.fn(() => new Promise<{ value?: Uint8Array; done: boolean }>((resolve) => {
    releasePendingRead = () => resolve({ value: undefined, done: true });
  }));

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

describe('chat stream route proxy cancellation', () => {
  beforeEach(() => {
    jest.clearAllMocks();
    mockRequireRuntimeUser.mockResolvedValue({
      user: { user_id: 'user-a' },
      response: null,
    } as never);
    mockGetChatHydrated.mockResolvedValue({
      chat: {
        id: 'chat-1',
        title: 'Chat',
        projectId: null,
        createdAt: '2026-06-07T00:00:00.000Z',
        updatedAt: '2026-06-07T00:00:00.000Z',
        archivedAt: null,
        model: 'sonnet-4.6-adaptive',
      },
      messages: [],
    });
    mockResolveBackendModelName.mockResolvedValue('backend-model');
    mockBeginStreamingMessage.mockReturnValue({
      userMessage: {
        id: 'user-1',
        role: 'user',
        content: 'hello',
        createdAt: '2026-06-07T00:00:00.000Z',
        status: 'complete',
      },
      assistantMessage: {
        id: 'assistant-1',
        role: 'assistant',
        content: '',
        createdAt: '2026-06-07T00:00:00.000Z',
        status: 'streaming',
      },
      sessionId: 'chat-1',
    });
  });

  it('cancels the backend SSE reader when the web client disconnects', async () => {
    const { POST } = await import('@/app/api/chats/[chatId]/stream/route');
    const backend = makeBackendStream();
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
      new Request('http://web.test/api/chats/chat-1/stream', {
        method: 'POST',
        body: JSON.stringify({
          content: 'hello',
          options: {
            model: 'sonnet-4.6-adaptive',
            webSearch: false,
            thinking: true,
            activeSkills: [],
          },
        }),
      }) as never,
      { params: Promise.resolve({ chatId: 'chat-1' }) },
    );

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    await reader?.read();
    await reader?.cancel();

    expect(backend.cancel).toHaveBeenCalled();
    expect(backend.releaseLock).toHaveBeenCalled();
    expect(mockUpdateStreamingAssistantMessage).not.toHaveBeenCalledWith(
      expect.anything(),
      expect.anything(),
      expect.anything(),
      expect.objectContaining({ status: 'failed' }),
    );
  });
});

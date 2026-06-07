jest.mock('@/lib/runtime-client', () => ({
  RuntimeClientError: class RuntimeClientError extends Error {},
  WebRuntimeClient: class WebRuntimeClient {},
  requireRuntimeClient: jest.fn(),
  getRuntimeClient: jest.fn(),
  readRuntimeErrorDetail: jest.fn(),
  runtimeErrorDetail: jest.fn((error: unknown) => (error instanceof Error ? error.message : String(error))),
}));

import { getStore, queueDeferredRunInput } from '@/lib/api/web-store';
import { requireRuntimeClient } from '@/lib/runtime-client';

const mockRequireRuntimeClient = requireRuntimeClient as jest.MockedFunction<typeof requireRuntimeClient>;

describe('queueDeferredRunInput', () => {
  beforeEach(() => {
    globalThis.__astraWebStores = undefined;
    mockRequireRuntimeClient.mockReset();
  });

  it('sends an explicit empty active_skills array so deferred turns can clear prior skill hints', async () => {
    const submitRunInput = jest.fn().mockResolvedValue({
      runId: 'run-1',
      accepted: true,
      duplicate: false,
    });
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        submitRunInput,
      },
    } as never);

    const store = getStore('user-a');
    store.chats.push({
      id: 'chat-1',
      title: 'Deferred test',
      projectId: null,
      createdAt: '2026-06-07T00:00:00.000Z',
      lastMessageAt: '2026-06-07T00:00:00.000Z',
      lastMessagePreview: 'hello',
      model: 'sonnet-4.6-adaptive',
      messages: [],
      activeRun: {
        runId: 'run-1',
        status: 'running',
        waitingFor: null,
      },
    });

    const result = await queueDeferredRunInput('user-a', 'chat-1', {
      content: 'clear previous skill constraints',
      options: {
        webSearch: false,
        thinking: true,
        model: 'sonnet-4.6-adaptive',
        activeSkills: [],
      },
    });

    expect(submitRunInput).toHaveBeenCalledWith('run-1', {
      idempotencyKey: expect.any(String),
      input: {
        content: 'clear previous skill constraints',
        active_skills: [],
      },
    });
    expect(result?.activeRun).toEqual({
      runId: 'run-1',
      status: 'input-queued',
      waitingFor: 'user_input',
    });
  });
});

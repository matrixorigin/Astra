jest.mock('@/lib/runtime-client', () => ({
  RuntimeClientError: class RuntimeClientError extends Error {},
  WebRuntimeClient: class WebRuntimeClient {},
  requireRuntimeClient: jest.fn(),
  getRuntimeClient: jest.fn(),
  readRuntimeErrorDetail: jest.fn(),
  runtimeErrorDetail: jest.fn((error: unknown) =>
    error instanceof Error ? error.message : String(error),
  ),
}));

import {
  StaleDeferredRunError,
  getChatHydrated,
  getStore,
  queueDeferredRunInput,
  resumeActiveRun,
  stopActiveRun,
} from '@/lib/api/web-store';
import { getRuntimeClient, requireRuntimeClient } from '@/lib/runtime-client';

const mockGetRuntimeClient = getRuntimeClient as jest.MockedFunction<
  typeof getRuntimeClient
>;
const mockRequireRuntimeClient = requireRuntimeClient as jest.MockedFunction<
  typeof requireRuntimeClient
>;

describe('queueDeferredRunInput', () => {
  beforeEach(() => {
    globalThis.__astraWebStores = undefined;
    mockGetRuntimeClient.mockReset();
    mockRequireRuntimeClient.mockReset();
  });

  it('sends an explicit empty active_skills array so deferred turns can clear prior skill hints', async () => {
    const submitRunInput = jest.fn().mockResolvedValue({
      runId: 'run-1',
      accepted: true,
      duplicate: false,
    });
    const getRunStatus = jest.fn().mockResolvedValue({
      runId: 'run-1',
      sessionId: 'chat-1',
      status: 'running',
      eventsCount: 1,
      waitingFor: null,
    });
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getRunStatus,
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
        source: 'local_mutation',
        observedAt: '2026-06-07T00:00:00.000Z',
      },
    });

    const result = await queueDeferredRunInput('user-a', 'chat-1', {
      content: 'clear previous skill constraints',
      pendingMessageId: 'user-input-1',
      options: {
        webSearch: false,
        thinking: true,
        model: 'sonnet-4.6-adaptive',
        activeSkills: [],
      },
    });

    expect(submitRunInput).toHaveBeenCalledWith('run-1', {
      idempotencyKey: 'web-deferred:run-1:user-input-1',
      input: {
        content: 'clear previous skill constraints',
        active_skills: [],
      },
    });
    expect(result?.userMessage.id).toBe('user-input-1');
    expect(result?.assistantMessage.role).toBe('assistant');
    expect(result?.activeRun).toEqual({
      runId: 'run-1',
      status: 'input-queued',
      waitingFor: 'user_input',
      assistantMessageId: result?.assistantMessage.id,
      nextEventIndex: 1,
    });
  });

  it('does not resubmit or duplicate messages for the same deferred input action', async () => {
    const submitRunInput = jest.fn().mockResolvedValue({
      runId: 'run-1',
      accepted: true,
      duplicate: false,
    });
    const getRunStatus = jest.fn().mockResolvedValue({
      runId: 'run-1',
      sessionId: 'chat-1',
      status: 'running',
      eventsCount: 4,
      waitingFor: null,
    });
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getRunStatus,
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
        source: 'local_mutation',
        observedAt: '2026-06-07T00:00:00.000Z',
      },
    });

    const first = await queueDeferredRunInput('user-a', 'chat-1', {
      content: 'continue from here',
      pendingMessageId: 'user-input-1',
    });
    const second = await queueDeferredRunInput('user-a', 'chat-1', {
      content: 'continue from here',
      pendingMessageId: 'user-input-1',
    });

    expect(submitRunInput).toHaveBeenCalledTimes(1);
    expect(second?.userMessage.id).toBe(first?.userMessage.id);
    expect(second?.assistantMessage.id).toBe(first?.assistantMessage.id);
    expect(store.chats[0].messages.map((message) => message.id)).toEqual([
      'user-input-1',
      first?.assistantMessage.id,
    ]);
  });

  it('does not orphan a local queued message when submitRunInput fails', async () => {
    const submitRunInput = jest
      .fn()
      .mockRejectedValue(new Error('runtime unavailable'));
    const getRunStatus = jest.fn().mockResolvedValue({
      runId: 'run-1',
      sessionId: 'chat-1',
      status: 'running',
      eventsCount: 1,
      waitingFor: null,
    });
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getRunStatus,
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
        source: 'local_mutation',
        observedAt: '2026-06-07T00:00:00.000Z',
      },
    });

    await expect(
      queueDeferredRunInput('user-a', 'chat-1', {
        content: 'do not orphan this',
        options: {
          webSearch: false,
          thinking: true,
          model: 'sonnet-4.6-adaptive',
          activeSkills: [],
        },
      }),
    ).rejects.toThrow('runtime unavailable');

    expect(store.chats[0].messages).toEqual([]);
    expect(store.chats[0].lastMessagePreview).toBe('hello');
    expect(store.chats[0].activeRun).toEqual({
      runId: 'run-1',
      status: 'running',
      waitingFor: null,
      nextEventIndex: 1,
      source: 'backend_poll',
      observedAt: expect.any(String),
    });
  });

  it('clears stale active runs before submitting deferred input', async () => {
    const submitRunInput = jest.fn();
    const getRunStatus = jest.fn().mockResolvedValue({
      runId: 'run-1',
      sessionId: 'chat-1',
      status: 'completed',
      eventsCount: 10,
      waitingFor: null,
    });
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getRunStatus,
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
        source: 'local_mutation',
        observedAt: '2026-06-07T00:00:00.000Z',
      },
    });

    await expect(
      queueDeferredRunInput('user-a', 'chat-1', {
        content: 'start a new turn instead',
      }),
    ).rejects.toBeInstanceOf(StaleDeferredRunError);

    expect(submitRunInput).not.toHaveBeenCalled();
    expect(store.chats[0].messages).toEqual([]);
    expect(store.chats[0].activeRun).toBeUndefined();
  });

  it('rejects oversized deferred input before calling the runtime', async () => {
    const submitRunInput = jest.fn();
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
        source: 'local_mutation',
        observedAt: '2026-06-07T00:00:00.000Z',
      },
    });

    await expect(
      queueDeferredRunInput('user-a', 'chat-1', {
        content: 'x'.repeat(20_001),
        options: {
          webSearch: false,
          thinking: true,
          model: 'sonnet-4.6-adaptive',
          activeSkills: [],
        },
      }),
    ).rejects.toThrow('Deferred input is too large.');

    expect(submitRunInput).not.toHaveBeenCalled();
    expect(store.chats[0].messages).toEqual([]);
  });

  it('hydrates a lost in-memory active run before submitting deferred input', async () => {
    const listRuntimeSessions = jest.fn().mockResolvedValue({
      sessions: [
        {
          session_id: 'chat-1',
          user_id: 'user-a',
          title: 'Deferred test',
          metadata: { source: 'web_v1' },
          status: 'active',
          created_at: '2026-06-07T00:00:00.000Z',
          updated_at: '2026-06-07T00:00:00.000Z',
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const listRuns = jest.fn().mockResolvedValue({
      runs: [
        {
          runId: 'run-recovered',
          sessionId: 'chat-1',
          status: 'running',
          waitingFor: null,
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const submitRunInput = jest.fn().mockResolvedValue({
      runId: 'run-recovered',
      accepted: true,
      duplicate: false,
    });
    const getRunStatus = jest.fn().mockResolvedValue({
      runId: 'run-recovered',
      sessionId: 'chat-1',
      status: 'running',
      eventsCount: 1,
      waitingFor: null,
    });

    mockGetRuntimeClient.mockResolvedValue({
      sdk: {
        listRuntimeSessions,
        listRuns,
      },
    } as never);
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: {
        getRunStatus,
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
      activeRun: undefined,
    });

    const result = await queueDeferredRunInput('user-a', 'chat-1', {
      content: 'recover before queueing',
      options: {
        webSearch: false,
        thinking: true,
        model: 'sonnet-4.6-adaptive',
        activeSkills: [],
      },
    });

    expect(listRuntimeSessions).toHaveBeenCalledWith({ limit: 200, offset: 0 });
    expect(listRuns).toHaveBeenCalledWith({ limit: 200, offset: 0 });
    expect(submitRunInput).toHaveBeenCalledWith('run-recovered', {
      idempotencyKey: expect.any(String),
      input: {
        content: 'recover before queueing',
        active_skills: [],
      },
    });
    expect(result?.activeRun).toEqual({
      runId: 'run-recovered',
      status: 'input-queued',
      waitingFor: 'user_input',
      assistantMessageId: result?.assistantMessage.id,
      nextEventIndex: 1,
    });
  });

  it('hydrates active runs when opening chat detail after in-memory store loss', async () => {
    const listRuntimeSessions = jest.fn().mockResolvedValue({
      sessions: [
        {
          session_id: 'chat-open',
          user_id: 'user-a',
          title: 'Open test',
          metadata: {
            source: 'web_v1',
            current_model: 'sonnet-4.6-adaptive',
          },
          status: 'active',
          created_at: '2026-06-07T00:00:00.000Z',
          updated_at: '2026-06-07T00:00:01.000Z',
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const listRuns = jest.fn().mockResolvedValue({
      runs: [
        {
          runId: 'run-open',
          sessionId: 'chat-open',
          status: 'input-queued',
          waitingFor: 'user_input',
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const getSessionTranscript = jest.fn().mockResolvedValue({
      items: [],
      total: 0,
      limit: 200,
      offset: 0,
    });

    mockGetRuntimeClient.mockResolvedValue({
      sdk: {
        listRuntimeSessions,
        listRuns,
        getSessionTranscript,
      },
    } as never);

    const result = await getChatHydrated('user-a', 'chat-open');

    expect(listRuntimeSessions).toHaveBeenCalledWith({ limit: 200, offset: 0 });
    expect(listRuns).toHaveBeenCalledWith({ limit: 200, offset: 0 });
    expect(getSessionTranscript).toHaveBeenCalledWith('chat-open', {
      limit: 200,
    });
    expect(result?.chat.id).toBe('chat-open');
    expect(result?.chat.model).toBe('sonnet-4.6-adaptive');
    expect(result?.activeRun).toEqual({
      runId: 'run-open',
      status: 'input-queued',
      waitingFor: 'user_input',
    });
  });

  it('hydrates a lost active run before stopping it', async () => {
    const listRuntimeSessions = jest.fn().mockResolvedValue({
      sessions: [
        {
          session_id: 'chat-stop',
          user_id: 'user-a',
          title: 'Stop test',
          metadata: { source: 'web_v1' },
          status: 'active',
          created_at: '2026-06-07T00:00:00.000Z',
          updated_at: '2026-06-07T00:00:00.000Z',
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const listRuns = jest.fn().mockResolvedValue({
      runs: [
        {
          runId: 'run-stop',
          sessionId: 'chat-stop',
          status: 'running',
          waitingFor: null,
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const cancelRun = jest.fn().mockResolvedValue(undefined);

    mockGetRuntimeClient.mockResolvedValue({
      sdk: { listRuntimeSessions, listRuns },
    } as never);
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: { cancelRun },
    } as never);

    getStore('user-a').chats.push({
      id: 'chat-stop',
      title: 'Stop test',
      projectId: null,
      createdAt: '2026-06-07T00:00:00.000Z',
      lastMessageAt: '2026-06-07T00:00:00.000Z',
      lastMessagePreview: 'hello',
      model: 'sonnet-4.6-adaptive',
      messages: [],
      activeRun: undefined,
    });

    const result = await stopActiveRun('user-a', 'chat-stop');

    expect(cancelRun).toHaveBeenCalledWith('run-stop');
    expect(result?.activeRun).toEqual({
      runId: 'run-stop',
      status: 'cancelling',
      waitingFor: null,
    });
  });

  it('keeps a cancelling active run before runtime cancellation resolves', async () => {
    const cancelRun = jest.fn(
      () =>
        new Promise<void>(() => {
          // Keep the runtime request pending to model a slow network/tool cancel.
        }),
    );
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: { cancelRun },
    } as never);

    const store = getStore('user-a');
    store.chats.push({
      id: 'chat-stop',
      title: 'Stop test',
      projectId: null,
      createdAt: '2026-06-07T00:00:00.000Z',
      lastMessageAt: '2026-06-07T00:00:00.000Z',
      lastMessagePreview: 'hello',
      model: 'sonnet-4.6-adaptive',
      messages: [],
      activeRun: {
        runId: 'run-stop',
        status: 'running',
        waitingFor: null,
        source: 'local_mutation',
        observedAt: '2026-06-07T00:00:00.000Z',
      },
    });

    const result = await stopActiveRun('user-a', 'chat-stop', {
      skipSync: true,
      cancelTimeoutMs: 1,
    });

    expect(cancelRun).toHaveBeenCalledWith('run-stop');
    expect(result).toEqual({
      activeRun: {
        runId: 'run-stop',
        status: 'cancelling',
        waitingFor: 'cancel_requested',
      },
      cancelPending: true,
    });
    expect(store.chats[0].activeRun).toMatchObject({
      runId: 'run-stop',
      status: 'cancelling',
      waitingFor: 'cancel_requested',
    });
  });

  it('clears cancelling state when a late runtime cancellation reaches terminal status', async () => {
    let resolveCancel!: () => void;
    const cancelRun = jest.fn(
      () =>
        new Promise<void>((resolve) => {
          resolveCancel = resolve;
        }),
    );
    const getRunStatus = jest.fn().mockResolvedValue({
      runId: 'run-stop',
      sessionId: 'chat-stop',
      status: 'cancelled',
      eventsCount: 3,
      waitingFor: null,
    });
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: { cancelRun },
    } as never);
    mockGetRuntimeClient.mockResolvedValue({
      sdk: { getRunStatus },
    } as never);

    const store = getStore('user-a');
    store.chats.push({
      id: 'chat-stop',
      title: 'Stop test',
      projectId: null,
      createdAt: '2026-06-07T00:00:00.000Z',
      lastMessageAt: '2026-06-07T00:00:00.000Z',
      lastMessagePreview: 'hello',
      model: 'sonnet-4.6-adaptive',
      messages: [],
      activeRun: {
        runId: 'run-stop',
        status: 'running',
        waitingFor: null,
        source: 'local_mutation',
        observedAt: '2026-06-07T00:00:00.000Z',
      },
    });

    const result = await stopActiveRun('user-a', 'chat-stop', {
      skipSync: true,
      cancelTimeoutMs: 1,
    });
    expect(result).toMatchObject({
      activeRun: {
        runId: 'run-stop',
        status: 'cancelling',
        waitingFor: 'cancel_requested',
      },
      cancelPending: true,
    });

    resolveCancel();
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(getRunStatus).toHaveBeenCalledWith('run-stop');
    expect(store.chats[0].activeRun).toBeUndefined();
  });

  it('does not resurrect a cancelling run as running while backend polling still reports it running', async () => {
    const cancelRun = jest.fn(
      () =>
        new Promise<void>(() => {
          // Keep cancellation pending so the next backend sync still sees running.
        }),
    );
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: { cancelRun },
    } as never);
    const listRuntimeSessions = jest.fn().mockResolvedValue({
      sessions: [
        {
          session_id: 'chat-stop',
          user_id: 'user-a',
          title: 'Stop test',
          metadata: { source: 'web_v1' },
          status: 'active',
          created_at: '2026-06-07T00:00:00.000Z',
          updated_at: '2026-06-07T00:00:00.000Z',
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const listRuns = jest.fn().mockResolvedValue({
      runs: [
        {
          runId: 'run-stop',
          sessionId: 'chat-stop',
          status: 'running',
          waitingFor: null,
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const getSessionTranscript = jest.fn().mockResolvedValue({
      items: [],
      total: 0,
      limit: 200,
      offset: 0,
    });
    mockGetRuntimeClient.mockResolvedValue({
      sdk: { listRuntimeSessions, listRuns, getSessionTranscript },
    } as never);

    const store = getStore('user-a');
    store.chats.push({
      id: 'chat-stop',
      title: 'Stop test',
      projectId: null,
      createdAt: '2026-06-07T00:00:00.000Z',
      lastMessageAt: '2026-06-07T00:00:00.000Z',
      lastMessagePreview: 'hello',
      model: 'sonnet-4.6-adaptive',
      messages: [],
      activeRun: {
        runId: 'run-stop',
        status: 'running',
        waitingFor: null,
        source: 'local_mutation',
        observedAt: '2026-06-07T00:00:00.000Z',
      },
    });

    await stopActiveRun('user-a', 'chat-stop', {
      skipSync: true,
      cancelTimeoutMs: 1,
    });

    const detail = await getChatHydrated('user-a', 'chat-stop');
    expect(detail?.activeRun).toEqual({
      runId: 'run-stop',
      status: 'cancelling',
      waitingFor: 'cancel_requested',
    });
    expect(listRuns).toHaveBeenCalled();
  });

  it('hydrates a lost paused run before resuming it', async () => {
    const listRuntimeSessions = jest.fn().mockResolvedValue({
      sessions: [
        {
          session_id: 'chat-resume',
          user_id: 'user-a',
          title: 'Resume test',
          metadata: { source: 'web_v1' },
          status: 'active',
          created_at: '2026-06-07T00:00:00.000Z',
          updated_at: '2026-06-07T00:00:00.000Z',
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const listRuns = jest.fn().mockResolvedValue({
      runs: [
        {
          runId: 'run-resume',
          sessionId: 'chat-resume',
          status: 'paused',
          waitingFor: null,
        },
      ],
      total: 1,
      limit: 200,
      offset: 0,
    });
    const resumeRun = jest.fn().mockResolvedValue(undefined);

    mockGetRuntimeClient.mockResolvedValue({
      sdk: { listRuntimeSessions, listRuns },
    } as never);
    mockRequireRuntimeClient.mockResolvedValue({
      sdk: { resumeRun },
    } as never);

    getStore('user-a').chats.push({
      id: 'chat-resume',
      title: 'Resume test',
      projectId: null,
      createdAt: '2026-06-07T00:00:00.000Z',
      lastMessageAt: '2026-06-07T00:00:00.000Z',
      lastMessagePreview: 'hello',
      model: 'sonnet-4.6-adaptive',
      messages: [],
      activeRun: undefined,
    });

    const result = await resumeActiveRun('user-a', 'chat-resume');

    expect(resumeRun).toHaveBeenCalledWith('run-resume');
    expect(result?.activeRun).toEqual({
      runId: 'run-resume',
      status: 'running',
      waitingFor: null,
    });
  });
});

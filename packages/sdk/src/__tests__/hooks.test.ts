/**
 * @jest-environment jsdom
 */
import { renderHook, act } from '@testing-library/react';
import { useAstraChat, useAstraRun } from '../hooks';
import { AstraClient } from '../client';
import type { SSEClient } from '../sse-client';
import type { StreamEvent, ConnectionState } from '../types';

// ─── Mock AstraClient ──────────────────────────────────────────────

function createMockClient() {
  const client = new AstraClient({ baseUrl: 'http://localhost' });

  // Mock streamChat to call onEvent callbacks synchronously
  const streamChatMock = jest.fn();
  client.streamChat = streamChatMock;

  // Mock run polling
  const getRunStatusMock = jest.fn();
  const getRunEventsMock = jest.fn();
  client.getRunStatus = getRunStatusMock;
  client.getRunEvents = getRunEventsMock;

  return { client, streamChatMock, getRunStatusMock, getRunEventsMock };
}

/**
 * Helper: set up streamChat mock to fire a sequence of events.
 * Returns a fake SSEClient with a close() spy.
 */
function mockStreamEvents(
  streamChatMock: jest.Mock,
  events: StreamEvent[],
) {
  const closeSpy = jest.fn();

  streamChatMock.mockImplementation(
    (_params: unknown, opts: { onEvent: (e: StreamEvent) => void; onStateChange?: (s: ConnectionState) => void }) => {
      // Fire state changes and events synchronously
      opts.onStateChange?.('connecting');
      opts.onStateChange?.('connected');
      for (const event of events) {
        opts.onEvent(event);
      }
      opts.onStateChange?.('disconnected');
      return { close: closeSpy } as unknown as SSEClient;
    },
  );

  return closeSpy;
}

// ─── useAstraChat ──────────────────────────────────────────────────

describe('useAstraChat', () => {
  test('initial state is idle with empty arrays', () => {
    const { client } = createMockClient();
    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    expect(result.current.sessionId).toBeNull();
    expect(result.current.runId).toBeNull();
    expect(result.current.messages).toEqual([]);
    expect(result.current.toolCalls).toEqual([]);
    expect(result.current.isStreaming).toBe(false);
    expect(result.current.error).toBeNull();
    expect(result.current.plan).toBeNull();
    expect(result.current.connectionState).toBe('idle');
    expect(result.current.agentEvents).toEqual([]);
  });

  test('sendMessage adds user + assistant placeholder', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, []);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hello');
    });

    // Should have user message + empty assistant placeholder
    expect(result.current.messages).toHaveLength(2);
    expect(result.current.messages[0].role).toBe('user');
    expect(result.current.messages[0].content).toBe('Hello');
    expect(result.current.messages[1].role).toBe('assistant');
    expect(result.current.messages[1].streaming).toBe(true);
  });

  test('processes session_info event', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: 'session_info', session_id: 'sess-1', run_id: 'run-1' } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    expect(result.current.sessionId).toBe('sess-1');
    expect(result.current.runId).toBe('run-1');
  });

  test('processes text_delta events to build assistant content', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: 'text_delta', content: 'Hello ' } as StreamEvent,
      { type: 'text_delta', content: 'World' } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    const assistantMsg = result.current.messages.find((m) => m.role === 'assistant');
    expect(assistantMsg?.content).toBe('Hello World');
  });

  test('processes tool_call_start and tool_call_end', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: 'tool_call_start',
        call_id: 'tc-1',
        tool: 'bash',
        arguments: '{"command":"ls"}',
      } as StreamEvent,
      {
        type: 'tool_call_end',
        call_id: 'tc-1',
        result: 'file1\nfile2',
      } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('list files');
    });

    expect(result.current.toolCalls).toHaveLength(1);
    expect(result.current.toolCalls[0].tool).toBe('bash');
    expect(result.current.toolCalls[0].status).toBe('done');
    expect(result.current.toolCalls[0].result).toBe('file1\nfile2');
  });

  test('processes usage event', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: 'usage',
        prompt_tokens: 100,
        completion_tokens: 50,
        cache_creation_tokens: 10,
        cache_read_tokens: 5,
      } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    expect(result.current.usage.promptTokens).toBe(100);
    expect(result.current.usage.completionTokens).toBe(50);
    expect(result.current.usage.totalTokens).toBe(150);
    expect(result.current.usage.cacheCreationTokens).toBe(10);
    expect(result.current.usage.cacheReadTokens).toBe(5);
  });

  test('processes error event', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: 'error', message: 'Server error' } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    expect(result.current.error).toBe('Server error');
  });

  test('run_finished finalizes assistant message', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: 'text_delta', content: 'done' } as StreamEvent,
      { type: 'run_finished', run_id: 'r1' } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    const lastMsg = result.current.messages[result.current.messages.length - 1];
    expect(lastMsg.streaming).toBe(false);
    expect(lastMsg.content).toBe('done');
    expect(result.current.isStreaming).toBe(false);
  });

  test('stop() aborts stream and finalizes', () => {
    const { client, streamChatMock } = createMockClient();
    // Don't fire run_finished so we stay in streaming state
    mockStreamEvents(streamChatMock, [
      { type: 'text_delta', content: 'partial' } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    // Still streaming (no run_finished)
    expect(result.current.isStreaming).toBe(true);

    act(() => {
      result.current.stop();
    });

    expect(result.current.isStreaming).toBe(false);
    expect(result.current.connectionState).toBe('idle');
  });

  test('reset() clears all state', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: 'session_info', session_id: 'sess-1' } as StreamEvent,
      { type: 'text_delta', content: 'text' } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    expect(result.current.messages.length).toBeGreaterThan(0);

    act(() => {
      result.current.reset();
    });

    expect(result.current.sessionId).toBeNull();
    expect(result.current.messages).toEqual([]);
    expect(result.current.isStreaming).toBe(false);
    expect(result.current.connectionState).toBe('idle');
  });

  test('agent events are tracked', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      { type: 'agent_delegated', agent_id: 'a1', role: 'coder', task: 'implement feature' } as StreamEvent,
      { type: 'agent_completed', agent_id: 'a1', status: 'completed', result: 'ok' } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    expect(result.current.agentEvents).toHaveLength(2);
    expect(result.current.agentEvents[0].type).toBe('agent_delegated');
    expect(result.current.agentEvents[1].type).toBe('agent_completed');
  });

  test('plan events create and update plan state', () => {
    const { client, streamChatMock } = createMockClient();
    mockStreamEvents(streamChatMock, [
      {
        type: 'plan_created',
        plan: {
          plan_id: 'p1',
          title: 'Test Plan',
          subtasks: [
            { id: 's1', title: 'Step 1' },
            { id: 's2', title: 'Step 2' },
          ],
        },
      } as unknown as StreamEvent,
      {
        type: 'plan_step_start',
        subtask_id: 's1',
        step: 's1',
      } as StreamEvent,
      {
        type: 'plan_step_done',
        subtask_id: 's1',
        step: 's1',
        result: 'success',
      } as StreamEvent,
    ]);

    const { result } = renderHook(() =>
      useAstraChat({ client }),
    );

    act(() => {
      result.current.sendMessage('Hi');
    });

    expect(result.current.plan).not.toBeNull();
    expect(result.current.plan?.title).toBe('Test Plan');
    expect(result.current.plan?.subtasks).toHaveLength(2);
    expect(result.current.plan?.subtasks[0].status).toBe('done');
    expect(result.current.plan?.subtasks[1].status).toBe('pending');
  });
});

// ─── useAstraRun ───────────────────────────────────────────────────

describe('useAstraRun', () => {
  beforeEach(() => {
    jest.useFakeTimers();
  });
  afterEach(() => {
    jest.useRealTimers();
  });

  test('polls run status on mount', async () => {
    const { client, getRunStatusMock, getRunEventsMock } = createMockClient();
    getRunStatusMock.mockResolvedValue({ status: 'running' });
    getRunEventsMock.mockResolvedValue([]);

    const { result } = renderHook(() =>
      useAstraRun({ client, runId: 'run-1' }),
    );

    // Flush the initial refresh
    await act(async () => {
      await jest.advanceTimersByTimeAsync(0);
    });

    expect(getRunStatusMock).toHaveBeenCalledWith('run-1');
    expect(result.current.status).toBe('running');
  });

  test('accumulates events from polling', async () => {
    const { client, getRunStatusMock, getRunEventsMock } = createMockClient();
    getRunStatusMock.mockResolvedValue({ status: 'running' });
    getRunEventsMock
      .mockResolvedValueOnce([
        { type: 'text_delta', content: 'a' },
      ])
      .mockResolvedValueOnce([
        { type: 'text_delta', content: 'b' },
      ])
      .mockResolvedValue([]);

    const { result } = renderHook(() =>
      useAstraRun({ client, runId: 'run-1', pollIntervalMs: 1000 }),
    );

    // First poll
    await act(async () => {
      await jest.advanceTimersByTimeAsync(0);
    });
    expect(result.current.events).toHaveLength(1);

    // Second poll
    await act(async () => {
      await jest.advanceTimersByTimeAsync(1000);
    });
    expect(result.current.events).toHaveLength(2);
  });

  test('sets error on polling failure', async () => {
    const { client, getRunStatusMock, getRunEventsMock } = createMockClient();
    getRunStatusMock.mockRejectedValue(new Error('Network error'));
    getRunEventsMock.mockRejectedValue(new Error('Network error'));

    const { result } = renderHook(() =>
      useAstraRun({ client, runId: 'run-1' }),
    );

    await act(async () => {
      await jest.advanceTimersByTimeAsync(0);
    });

    expect(result.current.error).toBe('Network error');
  });
});

import { renderHook, act } from '@testing-library/react';
import { useChatStream } from '@/hooks/use-chat-stream';
import type { ChatConfig } from '@/lib/workspace/types';
import { TextDecoder as NodeTextDecoder } from 'util';

// Polyfill TextDecoder for jsdom
if (typeof globalThis.TextDecoder === 'undefined') {
  Object.defineProperty(globalThis, 'TextDecoder', {
    value: NodeTextDecoder,
    configurable: true,
    writable: true,
  });
}

// ---------------------------------------------------------------------------
// Helpers — build a fake SSE Response using mock reader (no Web API deps)
// ---------------------------------------------------------------------------
function makeReader(chunks: Uint8Array[]) {
  let idx = 0;
  return {
    read: jest.fn(async () => {
      if (idx < chunks.length) {
        return { done: false, value: chunks[idx++] };
      }
      return { done: true, value: undefined };
    }),
  };
}

function sseResponse(events: Record<string, unknown>[], status = 200) {
  const text = events.map((e) => `data: ${JSON.stringify(e)}\n\n`).join('');
  // Use Buffer to encode (available in Node)
  const chunk = Buffer.from(text);
  const reader = makeReader(chunk.length > 0 ? [chunk] : []);
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: 'OK',
    body: { getReader: () => reader },
    text: () => Promise.resolve(text),
    headers: new Map([['content-type', 'text/event-stream']]),
  };
}

function sseResponseText(text: string, status = 200) {
  const chunk = Buffer.from(text);
  const reader = makeReader(chunk.length > 0 ? [chunk] : []);
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: 'OK',
    body: { getReader: () => reader },
    text: () => Promise.resolve(text),
    headers: new Map([['content-type', 'text/event-stream']]),
  };
}

function failResponse(status: number, body = '') {
  return {
    ok: false,
    status,
    statusText: 'Bad Request',
    body: null,
    text: () => Promise.resolve(body),
    headers: new Map(),
  };
}

// ---------------------------------------------------------------------------
// Test suite
// ---------------------------------------------------------------------------
describe('useChatStream', () => {
  const baseConfig: ChatConfig = { apiUrl: 'http://localhost:8000', sessionId: 'sess-1' };
  let fetchMock: jest.Mock;
  const originalFetch = globalThis.fetch;

  beforeEach(() => {
    fetchMock = jest.fn().mockResolvedValue(sseResponse([]));
    globalThis.fetch = fetchMock;
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  // ── Initial state ──────────────────────────────────────────────────────
  it('starts with no messages', () => {
    const { result } = renderHook(() => useChatStream(baseConfig));
    expect(result.current.messages).toEqual([]);
  });

  it('starts with isStreaming false', () => {
    const { result } = renderHook(() => useChatStream(baseConfig));
    expect(result.current.isStreaming).toBe(false);
  });

  it('starts with idle connectionState', () => {
    const { result } = renderHook(() => useChatStream(baseConfig));
    expect(result.current.connectionState).toBe('idle');
  });

  it('starts with the provided sessionId', () => {
    const { result } = renderHook(() => useChatStream(baseConfig));
    expect(result.current.sessionId).toBe('sess-1');
  });

  it('starts with null sessionId when none provided', () => {
    const { result } = renderHook(() => useChatStream({ apiUrl: 'http://localhost:8000' }));
    expect(result.current.sessionId).toBeNull();
  });

  it('starts with zero usage', () => {
    const { result } = renderHook(() => useChatStream(baseConfig));
    expect(result.current.usage.totalTokens).toBe(0);
  });

  it('starts with no followup suggestion', () => {
    const { result } = renderHook(() => useChatStream(baseConfig));
    expect(result.current.followupSuggestion).toBeNull();
  });

  // ── sendMessage ────────────────────────────────────────────────────────
  it('sendMessage adds user + assistant messages', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponse([{ type: 'turn_complete' }]),
    );

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Hello');
      // Allow micro-task queue to drain
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.messages).toHaveLength(2);
    expect(result.current.messages[0].role).toBe('user');
    expect(result.current.messages[0].content).toBe('Hello');
    expect(result.current.messages[1].role).toBe('assistant');
  });

  it('sendMessage sends POST to /api/backend/chat/stream', async () => {
    fetchMock.mockResolvedValueOnce(sseResponse([{ type: 'turn_complete' }]));

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Hi');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(fetchMock).toHaveBeenCalledWith(
      '/api/backend/chat/stream',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({ 'Content-Type': 'application/json' }),
      }),
    );
  });

  it('accumulates text deltas into assistant content', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponse([
        { type: 'text_delta', content: 'Hello' },
        { type: 'text_delta', content: ' world' },
        { type: 'turn_complete' },
      ]),
    );

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Hi');
      await new Promise((r) => setTimeout(r, 50));
    });

    const assistant = result.current.messages[1];
    expect(assistant.content).toBe('Hello world');
    expect(assistant.streaming).toBe(false);
  });

  it('derives a followup suggestion from the completed turn', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponse([
        { type: 'text_delta', content: 'Patched and verified.' },
        { type: 'tool_call_start', call_id: '1', tool: 'str_replace', arguments: '{}' },
        { type: 'tool_call_end', call_id: '1', result: 'ok' },
        { type: 'tool_call_start', call_id: '2', tool: 'run_build_test', arguments: '{}' },
        { type: 'tool_call_end', call_id: '2', result: 'ok' },
        { type: 'turn_complete' },
      ]),
    );

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Fix the bug');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.followupSuggestion).toBe('commit this');
  });

  it('prefers server followup suggestion from turn_complete', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponse([
        { type: 'text_delta', content: 'Patched and verified.' },
        { type: 'turn_complete', followup_suggestion: 'server says push it' },
      ]),
    );

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Fix the bug');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.followupSuggestion).toBe('server says push it');
  });

  it('processes a final buffered turn_complete without trailing delimiter', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponseText(
        'data: {"type":"text_delta","content":"Patched and verified."}\n\n' +
          'data: {"type":"turn_complete","followup_suggestion":"tail says ship it"}',
      ),
    );

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Hello');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.followupSuggestion).toBe('tail says ship it');
  });

  it('sets connectionState to error on fetch failure', async () => {
    fetchMock.mockResolvedValueOnce(failResponse(500, 'Internal Error'));

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Fail');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.connectionState).toBe('error');
    expect(result.current.error).toContain('500');
  });

  it('does not send when already streaming', async () => {
    // Return a stream that never finishes
    const neverResolve = new Promise<Response>(() => {});
    fetchMock.mockReturnValueOnce(neverResolve);

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('First');
    });

    // Now try sending again while still streaming
    const callCount = fetchMock.mock.calls.length;
    act(() => {
      result.current.sendMessage('Second');
    });

    expect(fetchMock.mock.calls.length).toBe(callCount);
  });

  // ── stop ───────────────────────────────────────────────────────────────
  it('stop marks message as not streaming', async () => {
    const neverResolve = new Promise<Response>(() => {});
    fetchMock.mockReturnValueOnce(neverResolve);

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Hello');
    });

    expect(result.current.isStreaming).toBe(true);

    act(() => {
      result.current.stop();
    });

    expect(result.current.isStreaming).toBe(false);
    expect(result.current.connectionState).toBe('idle');
    // The assistant message should be marked as not streaming
    const assistant = result.current.messages.find((m) => m.role === 'assistant');
    expect(assistant?.streaming).toBe(false);
  });

  // ── reset ──────────────────────────────────────────────────────────────
  it('reset clears all state', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponse([{ type: 'turn_complete' }]),
    );

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Hello');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.messages.length).toBeGreaterThan(0);

    act(() => {
      result.current.reset();
    });

    expect(result.current.messages).toEqual([]);
    expect(result.current.isStreaming).toBe(false);
    expect(result.current.error).toBeNull();
    expect(result.current.plan).toBeNull();
    expect(result.current.followupSuggestion).toBeNull();
    expect(result.current.connectionState).toBe('idle');
    expect(result.current.usage.totalTokens).toBe(0);
  });

  // ── session switching ──────────────────────────────────────────────────
  it('resets state when config.sessionId changes', async () => {
    fetchMock.mockResolvedValue(
      sseResponse([{ type: 'turn_complete' }]),
    );

    const { result, rerender } = renderHook(
      (props: ChatConfig) => useChatStream(props),
      { initialProps: baseConfig },
    );

    await act(async () => {
      result.current.sendMessage('Hello');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.messages.length).toBe(2);

    // Switch session
    await act(async () => {
      rerender({ ...baseConfig, sessionId: 'sess-2' });
    });

    expect(result.current.messages).toEqual([]);
    expect(result.current.sessionId).toBe('sess-2');
    expect(result.current.connectionState).toBe('idle');
  });

  // ── SSE event processing ──────────────────────────────────────────────
  it('processes session_info event', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponse([
        { type: 'session_info', session_id: 'new-sess', run_id: 'run-1' },
        { type: 'turn_complete' },
      ]),
    );

    const { result } = renderHook(() =>
      useChatStream({ apiUrl: 'http://localhost:8000' }),
    );

    await act(async () => {
      result.current.sendMessage('Hi');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.sessionId).toBe('new-sess');
    expect(result.current.runId).toBe('run-1');
  });

  it('processes usage events with accumulation', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponse([
        { type: 'usage', prompt_tokens: 10, completion_tokens: 5, cache_creation_tokens: 2, cache_read_tokens: 1 },
        { type: 'usage', prompt_tokens: 20, completion_tokens: 10, cache_creation_tokens: 3, cache_read_tokens: 4 },
        { type: 'turn_complete' },
      ]),
    );

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Hello');
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(result.current.usage.promptTokens).toBe(30);
    expect(result.current.usage.completionTokens).toBe(15);
    expect(result.current.usage.totalTokens).toBe(45);
    expect(result.current.usage.cacheCreationTokens).toBe(5);
    expect(result.current.usage.cacheReadTokens).toBe(5);
  });

  it('processes error event', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponse([{ type: 'error', message: 'Something went wrong' }]),
    );

    const { result } = renderHook(() => useChatStream(baseConfig));

    await act(async () => {
      result.current.sendMessage('Hi');
      await new Promise((r) => setTimeout(r, 50));
    });

    // The error message is captured even though the stream finishes afterward
    expect(result.current.error).toBe('Something went wrong');
    // After stream completes naturally, isStreaming is false
    expect(result.current.isStreaming).toBe(false);
  });
});

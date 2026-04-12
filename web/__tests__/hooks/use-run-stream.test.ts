import { renderHook, act } from '@testing-library/react';
import { TextDecoder as NodeTextDecoder } from 'util';

import { useRunStream } from '@/hooks/use-run-stream';

if (typeof globalThis.TextDecoder === 'undefined') {
  Object.defineProperty(globalThis, 'TextDecoder', {
    value: NodeTextDecoder,
    configurable: true,
    writable: true,
  });
}

function makeReader(chunks: Uint8Array[]) {
  let idx = 0;
  return {
    read: jest.fn(async () => {
      if (idx < chunks.length) {
        return { done: false, value: chunks[idx++] };
      }
      return { done: true, value: undefined };
    }),
    releaseLock: jest.fn(),
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
  };
}

describe('useRunStream', () => {
  const originalFetch = globalThis.fetch;
  let fetchMock: jest.Mock;

  beforeEach(() => {
    fetchMock = jest.fn();
    globalThis.fetch = fetchMock;
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  it('reconnects from the last seen event index', async () => {
    fetchMock
      .mockResolvedValueOnce(
        sseResponseText('data: {"type":"text_delta","content":"hello","index":7}\n\n'),
      )
      .mockResolvedValueOnce(
        sseResponseText('data: {"type":"text_delta","content":"again","index":8}\n\n'),
      );

    const { result } = renderHook(() =>
      useRunStream({ runId: 'run-1', autoConnect: true }),
    );

    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });

    await act(async () => {
      result.current.connect();
      await new Promise((r) => setTimeout(r, 50));
    });

    expect(String(fetchMock.mock.calls[0][0])).not.toContain('last_index=');
    expect(String(fetchMock.mock.calls[1][0])).toContain('/chat/runs/run-1/stream?last_index=8');
    expect(result.current.events.map((event) => event.content)).toEqual(['hello', 'again']);
  });
});

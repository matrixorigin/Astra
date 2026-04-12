import { TextDecoder as NodeTextDecoder } from 'util';

import { SSEClient } from '@/lib/streaming/sse-client';
import type { StreamEvent, ConnectionState } from '@/lib/streaming/types';

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

describe('SSEClient', () => {
  const originalFetch = globalThis.fetch;
  let fetchMock: jest.Mock;

  beforeEach(() => {
    fetchMock = jest.fn();
    globalThis.fetch = fetchMock;
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  it('processes a final buffered event without trailing delimiter', async () => {
    fetchMock.mockResolvedValueOnce(
      sseResponseText('data: {"type":"turn_complete","followup_suggestion":"tail works"}'),
    );

    const events: StreamEvent[] = [];
    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: 'http://example.test/sse',
      onEvent: (event) => events.push(event),
      onStateChange: (state) => states.push(state),
    });

    await client.connect();

    expect(events).toEqual([
      { type: 'turn_complete', followup_suggestion: 'tail works' },
    ]);
    expect(states).toEqual(['connecting', 'connected', 'disconnected']);
  });
});

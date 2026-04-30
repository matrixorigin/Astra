import { SSEClient, parseSseDataEvents } from '../../sse-client';
import type { ConnectionState, StreamEvent } from '../../types';

function byteStream(chunks: Uint8Array[]) {
  let i = 0;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (i < chunks.length) {
        controller.enqueue(chunks[i]);
        i++;
      } else {
        controller.close();
      }
    },
  });
}

function streamFromText(chunks: string[]) {
  const enc = new TextEncoder();
  return byteStream(chunks.map((chunk) => enc.encode(chunk)));
}

function streamResponse(body: ReadableStream<Uint8Array>, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : 'Error',
    body,
    headers: new Headers(),
    text: () => Promise.resolve(status === 200 ? '' : 'unavailable'),
  } as unknown as Response;
}

let originalFetch: typeof globalThis.fetch;
beforeEach(() => {
  originalFetch = globalThis.fetch;
});
afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe('scenarios / streaming resilience', () => {
  it('buffered SSE skips malformed blocks while preserving valid events before and after them', () => {
    const raw = [
      'data: {"type":"text_delta","content":"before"}',
      '',
      'data: {"type":"text_delta","content":',
      '',
      'data: {"type":"tool_call_start","call_id":"c1","tool":"grep","arguments":"{}"}',
      '',
      'data: {"type":"error","message":"tool failed","retryable":true,"retry_after_ms":25}',
      '',
      'data: {"type":"turn_complete"}',
      '',
    ].join('\n');

    const parsed = parseSseDataEvents(raw);
    expect(parsed.map((event) => event.type)).toEqual([
      'text_delta',
      'tool_call_start',
      'error',
      'turn_complete',
    ]);
    expect(parsed[2]).toMatchObject({
      type: 'error',
      message: 'tool failed',
      retryable: true,
      retry_after_ms: 25,
    });
  });

  it('stream reader reconstructs split UTF-8 and large text_delta payloads', async () => {
    const enc = new TextEncoder();
    const payload = `prefix-${'x'.repeat(128 * 1024)}-边云协同-🚀`;
    const frame = `data: ${JSON.stringify({ type: 'text_delta', content: payload })}\n\n`;
    const bytes = enc.encode(frame);
    const chunks = [
      bytes.slice(0, 17),
      bytes.slice(17, bytes.length - 11),
      bytes.slice(bytes.length - 11),
    ];
    globalThis.fetch = jest.fn().mockResolvedValue(streamResponse(byteStream(chunks)));

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: (event) => events.push(event),
    });
    await client.connect();

    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ type: 'text_delta', content: payload });
  });

  it('emits an error for a stream that ends after tool_call_start without tool_call_end', async () => {
    globalThis.fetch = jest.fn().mockResolvedValue(streamResponse(streamFromText([
      'data: {"type":"tool_call_start","call_id":"c1","tool":"bash","arguments":"{}"}\n\n',
      'data: {"type":"error","message":"edge executor disconnected","retryable":true}\n\n',
    ])));

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: (event) => events.push(event),
    });
    await client.connect();

    expect(events.map((event) => event.type)).toEqual(['tool_call_start', 'error']);
    expect(events[1]).toMatchObject({
      type: 'error',
      message: 'edge executor disconnected',
      retryable: true,
    });
  });

  it('retries one transient HTTP failure and then streams successfully', async () => {
    const states: ConnectionState[] = [];
    const events: StreamEvent[] = [];
    globalThis.fetch = jest
      .fn()
      .mockResolvedValueOnce(streamResponse(streamFromText([]), 503))
      .mockResolvedValueOnce(streamResponse(streamFromText([
        'data: {"type":"text_delta","content":"recovered"}\n\n',
        'data: {"type":"turn_complete"}\n\n',
      ])));

    const client = new SSEClient({
      url: 'http://localhost/stream',
      retryDelayMs: 1,
      maxRetries: 1,
      onStateChange: (state) => states.push(state),
      onEvent: (event) => events.push(event),
    });
    await client.connect();

    expect(globalThis.fetch).toHaveBeenCalledTimes(2);
    expect(states).toContain('error');
    expect(events[0]).toMatchObject({
      type: 'error',
      message: 'Connection error: unavailable',
      retryable: true,
    });
    expect(events.slice(1).map((event) => event.type)).toEqual(['text_delta', 'turn_complete']);
  });

  it('stops after maxRetries instead of resetting retry count on every reconnect', async () => {
    const events: StreamEvent[] = [];
    globalThis.fetch = jest
      .fn()
      .mockResolvedValue(streamResponse(streamFromText([]), 503));

    const client = new SSEClient({
      url: 'http://localhost/stream',
      retryDelayMs: 1,
      maxRetries: 2,
      onEvent: (event) => events.push(event),
    });
    await client.connect();

    expect(globalThis.fetch).toHaveBeenCalledTimes(3);
    expect(events.map((event) => event.type)).toEqual(['error', 'error', 'error']);
    expect(events.map((event) => (event.type === 'error' ? event.retryable : undefined))).toEqual([
      true,
      true,
      false,
    ]);
  });
});

import { SSEClient, parseSseDataEvents } from '../sse-client';
import type { StreamEvent, ConnectionState } from '../types';
import { readSseFixture } from './sse-fixture-helpers';

// ─── Mock Fetch + ReadableStream ────────────────────────────────────

function createMockStream(chunks: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let idx = 0;
  return new ReadableStream({
    pull(controller) {
      if (idx < chunks.length) {
        controller.enqueue(encoder.encode(chunks[idx]));
        idx++;
      } else {
        controller.close();
      }
    },
  });
}

function mockFetchStream(chunks: string[], status = 200) {
  return jest.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : 'Error',
    body: createMockStream(chunks),
    headers: new Headers(),
  } as unknown as Response);
}

function mockFetchNoBody(status = 200) {
  return jest.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    statusText: 'OK',
    body: null,
    headers: new Headers(),
  } as unknown as Response);
}

let originalFetch: typeof globalThis.fetch;
beforeEach(() => {
  originalFetch = globalThis.fetch;
});
afterEach(() => {
  globalThis.fetch = originalFetch;
});

// ─── Basic Connection ──────────────────────────────────────────────

describe('SSEClient — Connection', () => {
  test('connect sends Accept header and token', async () => {
    const chunks = ['data: {"type":"text_delta","content":"hi"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      token: 'my-token',
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(call[0]).toBe('http://localhost/stream');
    expect(call[1].headers['Accept']).toBe('text/event-stream');
    expect(call[1].headers['Authorization']).toBe('Bearer my-token');
    expect(call[1].headers['Cache-Control']).toBe('no-cache');
  });

  test('connect without token omits Authorization', async () => {
    const chunks = ['data: {"type":"text_delta","content":"hi"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: () => {},
    });
    await client.connect();

    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(call[1].headers['Authorization']).toBeUndefined();
  });

  test('custom headers are sent', async () => {
    const chunks = ['data: {"type":"text_delta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const client = new SSEClient({
      url: 'http://localhost/stream',
      headers: { 'X-Custom': 'value' },
      onEvent: () => {},
    });
    await client.connect();

    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(call[1].headers['X-Custom']).toBe('value');
  });

  test('POST method sends body and Content-Type', async () => {
    const chunks = ['data: {"type":"text_delta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const body = JSON.stringify({ message: 'hello' });
    const client = new SSEClient({
      url: 'http://localhost/stream',
      method: 'POST',
      body,
      onEvent: () => {},
    });
    await client.connect();

    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(call[1].method).toBe('POST');
    expect(call[1].body).toBe(body);
    expect(call[1].headers['Content-Type']).toBe('application/json');
  });
});

// ─── Event Parsing ─────────────────────────────────────────────────

describe('SSEClient — Event Parsing', () => {
  test('parses single SSE event', async () => {
    const chunks = ['data: {"type":"text_delta","content":"hello"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(1);
    expect(events[0].type).toBe('text_delta');
  });

  test('parses multiple events in one chunk', async () => {
    const chunks = [
      'data: {"type":"run_started","run_id":"r1"}\n\n' +
        'data: {"type":"text_delta","content":"a"}\n\n' +
        'data: {"type":"text_delta","content":"b"}\n\n',
    ];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(3);
    expect(events[0].type).toBe('run_started');
    expect(events[1].type).toBe('text_delta');
    expect(events[2].type).toBe('text_delta');
  });

  test('parses events split across chunks', async () => {
    // Event split in the middle
    const chunks = [
      'data: {"type":"text_del',
      'ta","content":"x"}\n\n',
    ];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(1);
    expect(events[0].type).toBe('text_delta');
  });

  test('ignores malformed JSON lines', async () => {
    const chunks = [
      'data: not-json\n\n' +
        'data: {"type":"text_delta","content":"ok"}\n\n',
    ];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(1);
    expect(events[0].type).toBe('text_delta');
  });

  test('handles data: with and without space prefix', async () => {
    const chunks = [
      'data:{"type":"text_delta","content":"no-space"}\n\n' +
        'data: {"type":"text_delta","content":"with-space"}\n\n',
    ];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(2);
  });

  test('calls onRawLine for each line', async () => {
    const chunks = ['data: {"type":"text_delta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const lines: string[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: () => {},
      onRawLine: (l) => lines.push(l),
    });
    await client.connect();

    expect(lines.length).toBeGreaterThanOrEqual(1);
    expect(lines[0]).toContain('data:');
  });
});

// ─── State Changes ─────────────────────────────────────────────────

describe('SSEClient — State Changes', () => {
  test('fires connecting → connected → disconnected', async () => {
    const chunks = ['data: {"type":"text_delta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: () => {},
      onStateChange: (s) => states.push(s),
    });
    await client.connect();

    expect(states[0]).toBe('connecting');
    expect(states[1]).toBe('connected');
    expect(states[2]).toBe('disconnected');
  });

  test('fires error state on non-OK response', async () => {
    globalThis.fetch = mockFetchStream([], 500);

    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: () => {},
      onStateChange: (s) => states.push(s),
      maxRetries: 0,
    });
    await client.connect();

    expect(states).toContain('error');
  });

  test('fires error state when body is null', async () => {
    globalThis.fetch = mockFetchNoBody(200);

    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: () => {},
      onStateChange: (s) => states.push(s),
      maxRetries: 0,
    });
    await client.connect();

    expect(states).toContain('error');
  });
});

// ─── Close / Abort ─────────────────────────────────────────────────

describe('SSEClient — Close', () => {
  test('close() fires disconnected state', () => {
    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: () => {},
      onStateChange: (s) => states.push(s),
    });
    client.close();
    expect(states).toContain('disconnected');
  });

  test('close() before connect is safe', () => {
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: () => {},
    });
    // Should not throw
    client.close();
  });

  test('close() during connect aborts fetch', async () => {
    let fetchAborted = false;
    globalThis.fetch = jest.fn().mockImplementation((_url: string, opts: { signal: AbortSignal }) => {
      opts.signal.addEventListener('abort', () => {
        fetchAborted = true;
      });
      // Return a promise that never resolves until aborted
      return new Promise((_resolve, reject) => {
        opts.signal.addEventListener('abort', () => {
          reject(new DOMException('aborted', 'AbortError'));
        });
      });
    });

    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: () => {},
      maxRetries: 0,
    });

    const connectPromise = client.connect();
    // Give connect time to call fetch
    await new Promise((r) => setTimeout(r, 10));
    client.close();
    await connectPromise;

    expect(fetchAborted).toBe(true);
  });
});

// ─── AbortSignal ───────────────────────────────────────────────────

describe('SSEClient — AbortSignal', () => {
  test('respects external AbortSignal', async () => {
    const controller = new AbortController();
    // Abort immediately
    controller.abort();

    globalThis.fetch = jest.fn().mockRejectedValue(new DOMException('aborted', 'AbortError'));

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'http://localhost/stream',
      onEvent: (e) => events.push(e),
      signal: controller.signal,
    });

    await client.connect();
    expect(events).toHaveLength(0);
  });
});

// ─── parseSseDataEvents ─────────────────────────────────────────────

describe('parseSseDataEvents', () => {
  test('parses multiple SSE blocks', () => {
    const raw =
      'data: {"type":"session_info","session_id":"s1","run_id":"r1"}\n\n' +
      'data: {"type":"text_delta","content":"hi"}\n\n';
    const events = parseSseDataEvents(raw);
    expect(events).toHaveLength(2);
    expect(events[0].type).toBe('session_info');
    expect(events[1]).toMatchObject({ type: 'text_delta', content: 'hi' });
  });

  test('parses __fixtures__/sse/workspace-stream.txt', () => {
    const events = parseSseDataEvents(readSseFixture('workspace-stream.txt'));
    expect(events.map((e) => e.type)).toEqual([
      'session_info',
      'text_delta',
      'text_delta',
      'usage',
      'turn_complete',
    ]);
  });
});

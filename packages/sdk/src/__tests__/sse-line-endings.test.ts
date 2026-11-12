import { afterEach, expect, test, vi } from 'vitest';
import { parseSseDataEvents, SSEClient } from '../sse-client';
import type { StreamEvent } from '../types';
import { AstraClient } from '../client';

afterEach(() => vi.unstubAllGlobals());
const events = [
  { type: 'text_delta', content: '中文🙂' },
  { type: 'run_finished', run_id: 'synthetic-run' },
] as StreamEvent[];

for (const [name, separator] of [['LF', '\n'], ['CRLF', '\r\n'], ['CR', '\r']]) {
  const raw = events.map((event) => `data: ${JSON.stringify(event)}${separator}${separator}`).join('');
  test(`buffered SSE preserves ${name} events`, () => {
    expect(parseSseDataEvents(raw)).toEqual(events);
  });
  for (const chunkSize of [1, 7, 999]) {
    test(`live SSE preserves ${name} events with ${chunkSize}-byte chunks`, async () => {
      const bytes = new TextEncoder().encode(raw);
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          for (let i = 0; i < bytes.length; i += chunkSize) {
            controller.enqueue(bytes.slice(i, i + chunkSize));
          }
          controller.close();
        },
      });
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(body)));
      const received: StreamEvent[] = [];
      const client = new SSEClient({
        url: 'https://example.test/stream', maxRetries: 0, requireTerminalEvent: true,
        onEvent: (event) => received.push(event),
      });
      await client.connect();
      client.close();
      expect(received).toEqual(events);
    });
  }
}

test('buffered and live parsing share mixed lines, BOM, comments, multiline data, and EOF tolerance', async () => {
  const raw = '\uFEFF: keepalive\r\nevent: ignored\r' +
    'data: {"type":\n' + 'data: "text_delta", "content":"中文🙂"}\r\n\r' +
    'data:{"type":"run_finished","run_id":"synthetic-run"}';
  expect(parseSseDataEvents(raw)).toEqual(events);
  const bytes = new TextEncoder().encode(raw);
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(new ReadableStream({
    start(controller) {
      for (const byte of bytes) controller.enqueue(Uint8Array.of(byte));
      controller.close();
    },
  }))));
  const received: StreamEvent[] = [];
  const lines: string[] = [];
  const client = new SSEClient({
    url: 'https://example.test/stream', maxRetries: 0, requireTerminalEvent: true,
    onEvent: (event) => received.push(event), onRawLine: (line) => lines.push(line),
  });
  await client.connect();
  client.close();
  expect(received).toEqual(events);
  expect(lines).toContain(': keepalive');
  expect(lines.every((line) => !/[\r\n]/.test(line))).toBe(true);
});

test('CR terminal is delivered before EOF or the next byte arrives', async () => {
  let source!: ReadableStreamDefaultController<Uint8Array>;
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(new ReadableStream({
    start(controller) { source = controller; },
  }))));
  const received: StreamEvent[] = [];
  let delivered!: () => void;
  const terminal = new Promise<void>((resolve) => { delivered = resolve; });
  const client = new SSEClient({
    url: 'https://example.test/stream', maxRetries: 0, requireTerminalEvent: true,
    onEvent: (event) => { received.push(event); delivered(); },
  });
  const pending = client.connect();
  source.enqueue(new TextEncoder().encode('data: {"type":"run_finished"}\r\r'));
  try {
    await terminal;
    expect(received).toEqual([{ type: 'run_finished' }]);
  } finally {
    source.close();
    await pending;
    client.close();
  }
});

for (const ending of ['', '\n', '\r', '\r\n', '\r\r']) {
  test(`terminal flushed after socket reset prevents retries (${JSON.stringify(ending)})`, async () => {
    const fetch = vi.fn().mockImplementation(() => {
      let sent = false;
      return Promise.resolve(new Response(new ReadableStream<Uint8Array>({
        pull(controller) {
          if (sent) controller.error(new Error('socket reset'));
          else {
            sent = true;
            controller.enqueue(new TextEncoder().encode(`data: {"type":"run_finished"}${ending}`));
          }
        },
      })));
    });
    vi.stubGlobal('fetch', fetch);
    const received: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'https://example.test/stream', maxRetries: 1, retryDelayMs: 1,
      requireTerminalEvent: true, onEvent: (event) => received.push(event),
    });
    await client.connect();
    client.close();
    expect(received).toEqual([{ type: 'run_finished' }]);
    expect(fetch).toHaveBeenCalledTimes(1);
  });
}

for (const count of [0, 1, 2]) {
  test(`all SSE entrypoints remove exactly one of ${count} leading BOMs`, async () => {
    const raw = '\uFEFF'.repeat(count) + 'data: {"type":"run_finished"}\n\n';
    const expected = count === 2 ? [] : [{ type: 'run_finished' }];
    expect(parseSseDataEvents(raw)).toEqual(expected);
    const bytes = new TextEncoder().encode(raw);
    const fetch = vi.fn().mockImplementation(() => Promise.resolve(new Response(bytes)));
    vi.stubGlobal('fetch', fetch);
    expect(await new AstraClient({ baseUrl: 'https://example.test' }).getRunEvents('synthetic-run'))
      .toEqual(expected);
    const received: StreamEvent[] = [];
    const client = new SSEClient({
      url: 'https://example.test/stream', maxRetries: 0, requireTerminalEvent: true,
      onEvent: (event) => received.push(event),
    });
    // Split each UTF-8 BOM across bytes, not just text chunks.
    fetch.mockImplementationOnce(() => Promise.resolve(new Response(new ReadableStream({
      start(controller) {
        for (const byte of bytes) controller.enqueue(Uint8Array.of(byte));
        controller.close();
      },
    }))));
    await client.connect();
    client.close();
    if (count === 2) {
      expect(received).toEqual([expect.objectContaining({ type: 'error', retryable: false })]);
    } else expect(received).toEqual(expected);
  });
}

test('raw line callbacks include logical blank separators', async () => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(': keepalive\r\ndata: {"type":"run_finished"}\r\n\r\n')));
  const lines: string[] = [];
  const client = new SSEClient({
    url: 'https://example.test/stream', maxRetries: 0,
    onEvent: () => {}, onRawLine: (line) => lines.push(line),
  });
  await client.connect();
  client.close();
  expect(lines).toEqual([': keepalive', 'data: {"type":"run_finished"}', '']);
});

for (const value of [null, true, 42, "text", [], {}, { type: null }, { type: 42 }]) {
  test(`buffered SSE rejects invalid event envelope ${JSON.stringify(value)}`, () => {
    const raw = `data: ${JSON.stringify(value)}\n\ndata: {"type":"run_finished"}\n\n`;
    expect(() => parseSseDataEvents(raw)).toThrow('Invalid SSE event envelope');
  });
}

test('SSE preserves future event types and additional fields', () => {
  const event = { type: 'future_event', future: { value: 1 } };
  expect(parseSseDataEvents(`data: ${JSON.stringify(event)}\n\n`)).toEqual([event]);
});

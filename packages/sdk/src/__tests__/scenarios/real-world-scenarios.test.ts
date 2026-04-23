/**
 * Long-chain "real usage" tests: AstraClient + mock fetch only (no real HTTP).
 * Sequences mirror typical workspace / edge / recovery flows.
 */
import { AstraApiError, AstraClient } from '../../client';
import { parseSseDataEvents } from '../../sse-client';
import { chatRunStreamPath } from '../../paths';
import type { StreamEvent } from '../../types';
import { readSseFixture, sseChunksForStreamMock } from '../sse-fixture-helpers';

function streamFrom(chunks: string[]) {
  const enc = new TextEncoder();
  let i = 0;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (i < chunks.length) {
        controller.enqueue(enc.encode(chunks[i]));
        i++;
      } else {
        controller.close();
      }
    },
  });
}

let originalFetch: typeof globalThis.fetch;
beforeEach(() => {
  originalFetch = globalThis.fetch;
});
afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe('scenarios / workspace stream', () => {
  it('streamChat receives session_info → text_deltas → usage → turn_complete in order', async () => {
    const order: string[] = [];
    const body = sseChunksForStreamMock(readSseFixture('workspace-stream.txt'));
    globalThis.fetch = jest.fn().mockResolvedValue({
      ok: true,
      status: 200,
      body: streamFrom(body),
      headers: new Headers(),
    } as unknown as Response);

    const client = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 'at' });
    const sse = client.streamChat({ message: 'hi' }, { onEvent: (e: StreamEvent) => order.push(e.type) });
    await new Promise((r) => setTimeout(r, 120));
    sse.close();
    expect(order).toEqual(['session_info', 'text_delta', 'text_delta', 'usage', 'turn_complete']);
  });
});

describe('scenarios / tool loop', () => {
  it('SSE carries tool_call_start → tool_call_end then turn_complete; then postToolResult succeeds', async () => {
    const seq: string[] = [];
    const sseChunks = [
      'data: {"type":"text_delta","content":"x"}\n\n',
      'data: {"type":"tool_call_start","call_id":"c1","tool":"grep","arguments":"{}"}\n\n',
      'data: {"type":"tool_call_end","call_id":"c1","result":"ok"}\n\n',
      'data: {"type":"turn_complete"}\n\n',
    ];
    const fetchImpl = jest
      .fn()
      // stream chat
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        body: streamFrom(sseChunks),
        headers: new Headers(),
      } as unknown as Response)
      // postToolResult
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        text: () => Promise.resolve('{}'),
        json: () => Promise.resolve({}),
        headers: new Headers(),
      } as unknown as Response);
    globalThis.fetch = fetchImpl;

    const client = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    const sse = client.streamChat({ message: 'run tool' }, { onEvent: (e: StreamEvent) => seq.push(e.type) });
    await new Promise((r) => setTimeout(r, 100));
    sse.close();
    expect(seq).toEqual(['text_delta', 'tool_call_start', 'tool_call_end', 'turn_complete']);

    await client.postToolResult({ request_id: 'r1', status: 'ok', output: 'out' });
    const toolUrl = (fetchImpl as jest.Mock).mock.calls[1][0] as string;
    expect(toolUrl).toContain('/tools/result');
  });
});

describe('scenarios / getRunEvents (buffered SSE) + last_index', () => {
  it('parses run replay body as StreamEvent list', async () => {
    const text =
      'data: {"type":"text_delta","content":"A"}\n\n' + 'data: {"type":"turn_complete"}\n\n';
    globalThis.fetch = jest.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: () => Promise.resolve(text),
      headers: new Headers(),
    } as unknown as Response);

    const client = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    const evs = await client.getRunEvents('run-1', 7);
    const url = (globalThis.fetch as jest.Mock).mock.calls[0][0] as string;
    expect(url).toContain(chatRunStreamPath('run-1'));
    expect(url).toContain('last_index=7');
    expect(evs.map((e: StreamEvent) => e.type)).toEqual(['text_delta', 'turn_complete']);
  });

  it('parseSseDataEvents round-trips the same as client buffer path', () => {
    const raw = 'data: {"type":"error","message":"late"}\n\n';
    const parsed = parseSseDataEvents(raw);
    expect(parsed).toHaveLength(1);
    expect(parsed[0].type).toBe('error');
  });
});

describe('scenarios / getRunEvents 401 + refresh (second fetch)', () => {
  it('401 on buffered run stream then refresh then retry returns SSE events and preserves last_index', async () => {
    const refresh = {
      access_token: 'new-access',
      refresh_token: 'new-refresh',
      token_type: 'Bearer',
      expires_in: 3600,
    };
    const sseText =
      'data: {"type":"text_delta","content":"x"}\n\n' + 'data: {"type":"turn_complete"}\n\n';
    let runStreamCalls = 0;
    globalThis.fetch = jest.fn().mockImplementation((url: string) => {
      const u = String(url);
      if (u.includes('/auth/refresh')) {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve(refresh),
          text: () => Promise.resolve(JSON.stringify(refresh)),
          headers: new Headers(),
        } as unknown as Response);
      }
      if (u.includes(chatRunStreamPath('run-reauth'))) {
        runStreamCalls++;
        if (runStreamCalls === 1) {
          return Promise.resolve({
            ok: false,
            status: 401,
            text: () => Promise.resolve('no'),
            json: () => Promise.resolve({}),
            headers: new Headers(),
          } as unknown as Response);
        }
        return Promise.resolve({
          ok: true,
          status: 200,
          text: () => Promise.resolve(sseText),
          headers: new Headers(),
        } as unknown as Response);
      }
      throw new Error(`unexpected fetch ${u}`);
    });

    const onRefresh = jest.fn();
    const client = new AstraClient({
      baseUrl: 'http://localhost:8000',
      accessToken: 'old',
      refreshToken: 'rtok',
      onTokenRefresh: onRefresh,
    });
    const evs = await client.getRunEvents('run-reauth', 3);
    expect(evs.map((e) => e.type)).toEqual(['text_delta', 'turn_complete']);
    expect(onRefresh).toHaveBeenCalled();
    const fetchMock = globalThis.fetch as jest.Mock;
    expect(fetchMock.mock.calls.length).toBe(3);
    const secondRunUrl = fetchMock.mock.calls[2][0] as string;
    expect(secondRunUrl).toContain('last_index=3');
  });
});

describe('scenarios / auth 401 + refresh (session happy path)', () => {
  it('getSession: 401 then refresh then retry returns session', async () => {
    const sessionWire = {
      session_id: 's1',
      user_id: 'u1',
      agent_id: null,
      title: null,
      status: 'active',
      event_count: 0,
      created_at: '2020-01-01',
      updated_at: null,
      ended_at: null,
      metadata: {},
    };
    const refresh = {
      access_token: 'new',
      refresh_token: 'nrt',
      token_type: 'Bearer',
      expires_in: 3600,
    };
    let sessionHits = 0;
    globalThis.fetch = jest.fn().mockImplementation((url: string) => {
      const u = String(url);
      if (u.includes('/auth/refresh')) {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve(refresh),
          text: () => Promise.resolve(JSON.stringify(refresh)),
          headers: new Headers(),
        } as unknown as Response);
      }
      if (u.includes('/sessions/s1') && !u.includes('audit') && !u.includes('activity')) {
        sessionHits++;
        if (sessionHits === 1) {
          return Promise.resolve({
            ok: false,
            status: 401,
            json: () => Promise.resolve({}),
            text: () => Promise.resolve('no'),
            headers: new Headers(),
          } as unknown as Response);
        }
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve(sessionWire),
          text: () => Promise.resolve(JSON.stringify(sessionWire)),
          headers: new Headers(),
        } as unknown as Response);
      }
      throw new Error(`unexpected fetch ${u}`);
    });

    const onRefresh = jest.fn();
    const client = new AstraClient({
      baseUrl: 'http://localhost:8000',
      accessToken: 'old',
      refreshToken: 'rtok',
      onTokenRefresh: onRefresh,
    });
    const s = await client.getSession('s1');
    expect(s.sessionId).toBe('s1');
    expect(onRefresh).toHaveBeenCalled();
  });
});

describe('scenarios / error ordering (turn_complete then error)', () => {
  it('emits both events: consumers must handle last-wins in UI', async () => {
    const types: string[] = [];
    const body = [
      'data: {"type":"turn_complete"}\n\n',
      'data: {"type":"error","message":"limit"}\n\n',
    ];
    globalThis.fetch = jest.fn().mockResolvedValue({
      ok: true,
      status: 200,
      body: streamFrom(body),
      headers: new Headers(),
    } as unknown as Response);

    const client = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    const sse = client.streamChat({ message: 'x' }, { onEvent: (e: StreamEvent) => types.push(e.type) });
    await new Promise((r) => setTimeout(r, 80));
    sse.close();
    expect(types).toEqual(['turn_complete', 'error']);
  });
});

describe('scenarios / getRunEvents HTTP error', () => {
  it('non-OK buffered stream throws AstraApiError', async () => {
    globalThis.fetch = jest.fn().mockResolvedValue({
      ok: false,
      status: 503,
      text: () => Promise.resolve('unavailable'),
      headers: new Headers(),
    } as unknown as Response);

    const client = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    await expect(client.getRunEvents('r1')).rejects.toBeInstanceOf(AstraApiError);
  });
});

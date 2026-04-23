import { AstraClient, chatRequestToWire } from '../client';
import { ASTRA_EDGE_ID_HEADER, PATH_AGENTS_EDGE, PATH_AGENTS_EDGE_HEARTBEAT, PATH_APPROVAL_RESPOND, PATH_CHAT_STREAM, joinApiPath, taskLeasePath, taskLeaseReleasePath, taskLeaseRenewPath } from '../paths';
import { SSEClient } from '../sse-client';

// ─── Mock stream (same idea as sse-client tests) ──────────────────

function createMockStream(chunks: string[]) {
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

describe('AstraClient — streamChat', () => {
  it('POSTs to /chat/stream with wire body, Bearer token, and yields SSE events', async () => {
    const events: string[] = [];
    const chunks = [
      'data: {"type":"session_info","session_id":"s1"}\n\n',
      'data: {"type":"text_delta","content":"hi"}\n\n',
    ];
    globalThis.fetch = jest.fn().mockResolvedValue({
      ok: true,
      status: 200,
      statusText: 'OK',
      body: createMockStream(chunks),
      headers: new Headers(),
    } as unknown as Response);

    const client = new AstraClient({
      baseUrl: 'http://localhost:8000',
      accessToken: 'tok-1',
    });
    const sse: SSEClient = client.streamChat(
      { message: 'hello', model: 'gpt' },
      {
        onEvent: (e) => {
          if (e.type === 'session_info' || e.type === 'text_delta') events.push(e.type);
        },
      },
    );

    await new Promise((r) => setTimeout(r, 80));
    sse.close();

    const [url, init] = (globalThis.fetch as jest.Mock).mock.calls[0] as [string, RequestInit];
    expect(String(url).endsWith(PATH_CHAT_STREAM)).toBe(true);
    expect(init.method).toBe('POST');
    const headers = init.headers as Record<string, string>;
    expect(headers['Authorization']).toBe('Bearer tok-1');
    expect(init.body).toBe(
      JSON.stringify(
        chatRequestToWire({
          message: 'hello',
          model: 'gpt',
        }),
      ),
    );
    expect(events).toEqual(['session_info', 'text_delta']);
  });

  it('applies pathPrefix to stream URL', async () => {
    globalThis.fetch = jest.fn().mockResolvedValue({
      ok: true,
      status: 200,
      body: createMockStream([]),
      headers: new Headers(),
    } as unknown as Response);

    const client = new AstraClient({
      baseUrl: 'http://localhost:8000',
      pathPrefix: '/api',
      accessToken: 'x',
    });
    const sse = client.streamChat({ message: 'a' }, { onEvent: () => {} });
    await new Promise((r) => setTimeout(r, 30));
    sse.close();
    const url = (globalThis.fetch as jest.Mock).mock.calls[0][0] as string;
    expect(url).toBe('http://localhost:8000' + joinApiPath('/api', PATH_CHAT_STREAM));
  });
});

describe('AstraClient — thin edge / approval / task lease', () => {
  function okFetch() {
    return jest.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: () => Promise.resolve('{}'),
      json: () => Promise.resolve({}),
      headers: new Headers(),
    } as unknown as Response);
  }

  it('postApprovalRespond posts to /approval/respond with JSON', async () => {
    globalThis.fetch = okFetch();
    const c = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    await c.postApprovalRespond({ request_id: 'r1', decision: 'allow' });
    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(String(call[0]).endsWith(PATH_APPROVAL_RESPOND)).toBe(true);
    expect((call[1] as { method: string }).method).toBe('POST');
    expect(JSON.parse((call[1] as { body: string }).body as string)).toEqual({ request_id: 'r1', decision: 'allow' });
  });

  it('registerEdge POST /agents/edge and optional X-Astra-Edge-Id', async () => {
    globalThis.fetch = okFetch();
    const c = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    await c.registerEdge(
      { edge_agent_id: 'e1' },
      { edgeTransportId: 'transport-z' },
    );
    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(String(call[0]).endsWith(PATH_AGENTS_EDGE)).toBe(true);
    expect((call[1] as { headers: Record<string, string> }).headers[ASTRA_EDGE_ID_HEADER]).toBe('transport-z');
  });

  it('postEdgeHeartbeat POST /agents/edge/heartbeat and optional X-Astra-Edge-Id', async () => {
    globalThis.fetch = okFetch();
    const c = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    await c.postEdgeHeartbeat({ edge_agent_id: 'e1' }, { edgeTransportId: 'tr' });
    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(String(call[0]).endsWith(PATH_AGENTS_EDGE_HEARTBEAT)).toBe(true);
    expect((call[1] as { headers: Record<string, string> }).headers[ASTRA_EDGE_ID_HEADER]).toBe('tr');
  });

  it('getTaskLease GET /tasks/{id}/lease', async () => {
    globalThis.fetch = okFetch();
    const c = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    await c.getTaskLease('task-9');
    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(String(call[0]).endsWith(taskLeasePath('task-9'))).toBe(true);
  });

  it('postTaskLeaseRelease with edge header and body', async () => {
    globalThis.fetch = okFetch();
    const c = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    await c.postTaskLeaseRelease(
      'task-a',
      { edge_agent_id: 'ea' },
      { edgeTransportId: 'tport' },
    );
    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(String(call[0]).endsWith(taskLeaseReleasePath('task-a'))).toBe(true);
    expect((call[1] as { method: string }).method).toBe('POST');
    expect((call[1] as { headers: Record<string, string> }).headers[ASTRA_EDGE_ID_HEADER]).toBe('tport');
  });

  it('postTaskLeaseRenew with edge header and body', async () => {
    globalThis.fetch = okFetch();
    const c = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 't' });
    await c.postTaskLeaseRenew('task-b', { edge_agent_id: 'eb', ttl_sec: 30 }, { edgeTransportId: 't2' });
    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(String(call[0]).endsWith(taskLeaseRenewPath('task-b'))).toBe(true);
    expect(JSON.parse((call[1] as { body: string }).body as string)).toEqual({ edge_agent_id: 'eb', ttl_sec: 30 });
  });
});

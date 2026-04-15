import { AstraWebSocket } from '../websocket';

// ─── Mock WebSocket ────────────────────────────────────────────────

class MockWebSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;

  readyState = MockWebSocket.CONNECTING;
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((e: { data: string }) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  sent: string[] = [];
  url: string;
  protocols?: string | string[];

  constructor(url: string, protocols?: string | string[]) {
    this.url = url;
    this.protocols = protocols;
    // Auto-open after microtask to simulate real WS
    setTimeout(() => {
      this.readyState = MockWebSocket.OPEN;
      this.onopen?.();
    }, 0);
  }

  send(data: string) {
    this.sent.push(data);
  }

  close() {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.();
  }

  // Test helper: simulate server message
  _receive(data: unknown) {
    this.onmessage?.({ data: JSON.stringify(data) });
  }
}

// Patch global
const origWS = globalThis.WebSocket;
beforeAll(() => {
  (globalThis as any).WebSocket = MockWebSocket;
});
afterAll(() => {
  (globalThis as any).WebSocket = origWS;
});

// ─── Tests ──────────────────────────────────────────────────────────

describe('AstraWebSocket', () => {
  test('connect() resolves when WebSocket opens', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();
    expect(ws.connectionState).toBe('connected');
  });

  test('emits events via .on()', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    const events: any[] = [];
    ws.on('event', (e) => events.push(e));

    // Simulate server event
    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'text_delta', delta: 'hello' });

    expect(events).toHaveLength(1);
    expect(events[0].type).toBe('text_delta');
  });

  test('emits type-specific events', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    const deltas: any[] = [];
    ws.on('text_delta', (e) => deltas.push(e));

    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'text_delta', delta: 'a' });
    raw._receive({ type: 'run_started', runId: 'r1' });
    raw._receive({ type: 'text_delta', delta: 'b' });

    expect(deltas).toHaveLength(2);
  });

  test('off() removes listener', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    const events: any[] = [];
    const handler = (e: any) => events.push(e);
    ws.on('event', handler);

    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'text_delta', delta: 'a' });
    ws.off('event', handler);
    raw._receive({ type: 'text_delta', delta: 'b' });

    expect(events).toHaveLength(1);
  });

  test('sendMessage sends correct JSON', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    ws.sendMessage('hello', { sessionId: 's1', model: 'gpt-4' });

    const raw = (ws as any).ws as MockWebSocket;
    const sent = JSON.parse(raw.sent[0]);
    expect(sent).toEqual({
      type: 'message',
      content: 'hello',
      session_id: 's1',
      model: 'gpt-4',
    });
  });

  test('cancelRun sends cancel message', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    ws.cancelRun('r1');

    const raw = (ws as any).ws as MockWebSocket;
    const sent = JSON.parse(raw.sent[0]);
    expect(sent).toEqual({ type: 'cancel_run', run_id: 'r1' });
  });

  test('pauseRun and resumeRun', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    ws.pauseRun('r1');
    ws.resumeRun('r1');

    const raw = (ws as any).ws as MockWebSocket;
    expect(JSON.parse(raw.sent[0])).toEqual({ type: 'pause_run', run_id: 'r1' });
    expect(JSON.parse(raw.sent[1])).toEqual({ type: 'resume_run', run_id: 'r1' });
  });

  test('approveToolCall sends approval', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    ws.approveToolCall({ callId: 'req1', approved: true });

    const raw = (ws as any).ws as MockWebSocket;
    const sent = JSON.parse(raw.sent[0]);
    expect(sent).toEqual({
      type: 'tool_approval',
      request_id: 'req1',
      approved: true,
    });
  });

  test('tracks sessionId from session_info event', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'session_info', session_id: 'abc' });

    expect(ws.sessionId).toBe('abc');
  });

  test('tracks runId from run_started/run_finished', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'run_started', run_id: 'r1' });
    expect(ws.runId).toBe('r1');

    raw._receive({ type: 'run_finished', run_id: 'r1' });
    expect(ws.runId).toBeNull();
  });

  test('close closes WebSocket', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    await ws.connect();

    ws.close();
    expect(ws.connectionState).toBe('disconnected');
  });

  test('legacy onEvent callback fires', async () => {
    const events: any[] = [];
    const ws = new AstraWebSocket({
      url: 'ws://localhost/ws',
      onEvent: (e) => events.push(e),
    });
    await ws.connect();

    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'text_delta', delta: 'x' });

    expect(events).toHaveLength(1);
  });

  test('stateChange events fire', async () => {
    const states: string[] = [];
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws' });
    ws.on('stateChange', (s) => states.push(s));

    await ws.connect();
    expect(states).toContain('connected');

    ws.close();
    expect(states).toContain('disconnected');
  });
});

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
    if (JSON.parse(data).type === 'auth') {
      setTimeout(() => this._receive({ type: 'auth_ok', interaction_api_major: '3' }), 0);
    }
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

async function nextAttachedSocket(client: AstraWebSocket, previous: MockWebSocket): Promise<MockWebSocket> {
  for (let attempt = 0; attempt < 100; attempt++) {
    const socket = (client as any).ws as MockWebSocket;
    if (socket !== previous && socket?.sent.some((frame) => JSON.parse(frame).type === 'attach_run')) {
      return socket;
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error('reconnect did not send attach_run');
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
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();
    expect(ws.connectionState).toBe('connected');
  });

  test('emits events via .on()', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
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
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
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
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
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
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    ws.sendMessage('hello', {
      sessionId: 's1',
      modelSelection: { offeringId: 'offer-gpt-4' },
    });

    const raw = (ws as any).ws as MockWebSocket;
    const sent = JSON.parse(raw.sent.at(-1)!);
    expect(sent).toEqual({
      type: 'message',
      content: 'hello',
      session_id: 's1',
      model_selection: { offering_id: 'offer-gpt-4' },
    });
  });

  test('sendMessage rejects client route authority', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    expect(() => ws.sendMessage('hello', {
      modelSelection: { offeringId: 'offer-kimi', gateway: 'primary' } as any,
    })).toThrow("modelSelection contains unsupported field 'gateway'");
  });

  test('sendMessage requires an exact Offering id', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    expect(() => ws.sendMessage('hello', {} as any)).toThrow('modelSelection.offeringId is required');
    expect(() => ws.sendMessage('hello', { modelSelection: { offeringId: '' } })).toThrow(
      'modelSelection.offeringId is required',
    );
    expect(() => ws.sendMessage('hello', { modelSelection: { offeringId: ' offer-kimi' } })).toThrow(
      'modelSelection.offeringId must be an exact identifier',
    );
  });

  test('cancelRun sends cancel message', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    ws.cancelRun('r1');

    const raw = (ws as any).ws as MockWebSocket;
    const sent = JSON.parse(raw.sent.at(-1)!);
    expect(sent).toEqual({ type: 'cancel_run', run_id: 'r1' });
  });

  test('pauseRun and resumeRun', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    ws.pauseRun('r1');
    ws.resumeRun('r1');

    const raw = (ws as any).ws as MockWebSocket;
    expect(JSON.parse(raw.sent[1])).toEqual({ type: 'pause_run', run_id: 'r1' });
    expect(JSON.parse(raw.sent[2])).toEqual({ type: 'resume_run', run_id: 'r1' });
  });

  test('authenticates in the first frame without placing the token in the URL', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'secret' });
    await ws.connect();
    const raw = (ws as any).ws as MockWebSocket;
    expect(raw.url).toBe('ws://localhost/ws');
    expect(JSON.parse(raw.sent[0])).toEqual({
      type: 'auth', token: 'Bearer secret', interaction_api_major: '3',
    });
  });

  test('reconnects and reattaches at the last durable event index', async () => {
    const ws = new AstraWebSocket({
      url: 'ws://localhost/ws', token: 'test-token', reconnectDelayMs: 1,
    });
    const observed: string[] = [];
    ws.on('event', (event) => observed.push(event.type));
    await ws.connect();
    const first = (ws as any).ws as MockWebSocket;
    first._receive({ type: 'run_started', run_id: 'r1' });
    first._receive({ type: 'text_delta', index: 4, content: 'first' });
    first.close();
    const next = await nextAttachedSocket(ws, first);
    expect(next).not.toBe(first);
    expect(JSON.parse(next.sent[0]).type).toBe('auth');
    expect(JSON.parse(next.sent[1])).toEqual({
      type: 'attach_run', run_id: 'r1', last_index: 4,
    });
    next._receive({ type: 'session_info', session_id: 's1', run_id: 'r1' });
    next._receive({ type: 'text_delta', index: 4, content: 'first' });
    next._receive({ type: 'usage', index: 4, prompt_tokens: 1 });
    next._receive({ type: 'text_delta', index: 5, content: 'next' });
    expect(ws.runId).toBe('r1');
    expect(observed).toEqual([
      'run_started', 'text_delta', 'session_info', 'usage', 'text_delta',
    ]);
    ws.approveToolCall({ callId: 'approval-1', approved: true });
    ws.respondToUserPrompt('prompt-1', [
      { question: 'Continue?', answers: ['yes'], multi_select: false },
    ]);
    expect(JSON.parse(next.sent[2])).toEqual({
      type: 'tool_approval', request_id: 'approval-1', approved: true,
    });
    expect(JSON.parse(next.sent[3])).toEqual({
      type: 'user_prompt', request_id: 'prompt-1',
      answers: { answers: [{ question: 'Continue?', answers: ['yes'], multi_select: false }] },
    });
    ws.close();
  });

  test('delivers the deferred terminal once after a higher-index Explain publication', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    const finished: any[] = [];
    ws.on('run_finished', (event) => finished.push(event));
    await ws.connect();
    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'run_started', run_id: 'run-explain' });
    raw._receive({
      type: 'artifact_publication', index: 1, run_id: 'run-explain',
      schema_version: 1, turn_id: 'turn-1', execution_owner_generation: 7,
      artifact_type: 'explain_analyze_snapshot', recorded: true,
      status: 'unavailable', reason_code: 'report_missing', message: 'Report unavailable.',
    });
    const terminal = {
      type: 'run_finished', index: 0, run_id: 'run-explain',
      owner_generation: 7, status: 'completed',
    };
    raw._receive(terminal);
    raw._receive(terminal);
    expect(finished).toEqual([terminal]);
    expect(ws.runId).toBeNull();
  });

  test('replays the original terminal identity after disconnect following publication', async () => {
    const ws = new AstraWebSocket({
      url: 'ws://localhost/ws', token: 'test-token', reconnectDelayMs: 1,
    });
    const observed: any[] = [];
    ws.on('event', (event) => observed.push(event));
    await ws.connect();
    const first = (ws as any).ws as MockWebSocket;
    first._receive({ type: 'run_started', run_id: 'run-explain' });
    const publication = {
      type: 'artifact_publication', index: 1, run_id: 'run-explain',
      schema_version: 1, turn_id: 'turn-1', execution_owner_generation: 7,
      artifact_type: 'explain_analyze_snapshot', recorded: true,
      status: 'unavailable', reason_code: 'report_missing', message: 'Report unavailable.',
    };
    first._receive(publication);
    first.close();
    const next = await nextAttachedSocket(ws, first);
    expect(JSON.parse(next.sent[1])).toEqual({
      type: 'attach_run', run_id: 'run-explain', last_index: 0,
    });
    next._receive({ type: 'session_info', session_id: 's1', run_id: 'run-explain' });
    next._receive(publication);
    const terminal = {
      type: 'run_finished', index: 0, run_id: 'run-explain',
      owner_generation: 7, status: 'completed',
    };
    next._receive(terminal);
    expect(observed.filter((event) => event.type === 'artifact_publication')).toEqual([publication]);
    expect(observed.filter((event) => event.type === 'run_finished')).toEqual([terminal]);
    expect(ws.runId).toBeNull();
  });

  test('retries a transient attach failure and resumes observation', async () => {
    const ws = new AstraWebSocket({
      url: 'ws://localhost/ws', token: 'test-token', reconnectDelayMs: 1,
    });
    await ws.connect();
    const first = (ws as any).ws as MockWebSocket;
    first._receive({ type: 'run_started', run_id: 'r1' });
    first._receive({ type: 'text_delta', index: 4, content: 'before' });
    first.close();
    const failed = await nextAttachedSocket(ws, first);
    expect(JSON.parse(failed.sent[1])).toEqual({
      type: 'attach_run', run_id: 'r1', last_index: 4,
    });
    failed._receive({ type: 'error', code: 'UPSTREAM_ERROR', retryable: true, message: 'DB busy' });
    expect(failed.readyState).toBe(MockWebSocket.CLOSED);
    const recovered = await nextAttachedSocket(ws, failed);
    expect(recovered).not.toBe(failed);
    expect(JSON.parse(recovered.sent[1])).toEqual({
      type: 'attach_run', run_id: 'r1', last_index: 4,
    });
    recovered._receive({ type: 'session_info', session_id: 's1', run_id: 'r1' });
    expect(ws.connectionState).toBe('connected');
    expect(ws.runId).toBe('r1');
    ws.approveToolCall({ callId: 'approval-1', approved: true });
    expect(JSON.parse(recovered.sent[2]).type).toBe('tool_approval');
    ws.close();
  });

  test('bounds reconnect attempts across transient attach failures', async () => {
    const ws = new AstraWebSocket({
      url: 'ws://localhost/ws', token: 'test-token', reconnectDelayMs: 1,
      maxReconnectAttempts: 1,
    });
    await ws.connect();
    const first = (ws as any).ws as MockWebSocket;
    first._receive({ type: 'run_started', run_id: 'r1' });
    first.close();
    const failed = await nextAttachedSocket(ws, first);
    failed._receive({ type: 'error', code: 'UPSTREAM_ERROR', retryable: true, message: 'DB busy' });
    await new Promise((resolve) => setTimeout(resolve, 25));
    expect((ws as any).ws).toBe(failed);
    expect(ws.connectionState).toBe('disconnected');
    expect(ws.runId).toBe('r1');
    ws.close();
  });

  test('rejects a missing run and accepts a valid attach on the same connection', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();
    const raw = (ws as any).ws as MockWebSocket;
    const missing = ws.attachRun('missing', 3);
    raw._receive({ type: 'error', code: 'NOT_FOUND', retryable: false, message: 'Run not found' });
    await expect(missing).rejects.toThrow('Run not found');
    expect(ws.runId).toBeNull();
    expect(ws.connectionState).toBe('connected');
    const owned = ws.attachRun('owned', 7);
    expect(JSON.parse(raw.sent.at(-1)!)).toEqual({
      type: 'attach_run', run_id: 'owned', last_index: 7,
    });
    raw._receive({ type: 'session_info', session_id: 's1', run_id: 'owned' });
    await owned;
    expect(ws.runId).toBe('owned');
    ws.close();
  });

  test('clears an auto-reattach rejection so another run can attach', async () => {
    const ws = new AstraWebSocket({
      url: 'ws://localhost/ws', token: 'test-token', reconnectDelayMs: 1,
    });
    await ws.connect();
    const first = (ws as any).ws as MockWebSocket;
    first._receive({ type: 'run_started', run_id: 'missing' });
    first.close();
    const next = await nextAttachedSocket(ws, first);
    next._receive({ type: 'error', code: 'NOT_FOUND', retryable: false, message: 'Run not found' });
    expect(ws.runId).toBeNull();
    expect(ws.connectionState).toBe('connected');
    const owned = ws.attachRun('owned');
    next._receive({ type: 'session_info', session_id: 's1', run_id: 'owned' });
    await owned;
    expect(ws.runId).toBe('owned');
    ws.close();
  });

  test('approveToolCall sends approval', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    ws.approveToolCall({ callId: 'req1', approved: true });

    const raw = (ws as any).ws as MockWebSocket;
    const sent = JSON.parse(raw.sent.at(-1)!);
    expect(sent).toEqual({
      type: 'tool_approval',
      request_id: 'req1',
      approved: true,
    });
  });

  test('tracks sessionId from session_info event', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'session_info', session_id: 'abc' });

    expect(ws.sessionId).toBe('abc');
  });

  test('tracks runId from run_started/run_finished', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'run_started', run_id: 'r1' });
    expect(ws.runId).toBe('r1');

    raw._receive({ type: 'run_finished', run_id: 'r1' });
    expect(ws.runId).toBeNull();
  });

  test('close closes WebSocket', async () => {
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    await ws.connect();

    ws.close();
    expect(ws.connectionState).toBe('disconnected');
  });

  test('legacy onEvent callback fires', async () => {
    const events: any[] = [];
    const ws = new AstraWebSocket({
      url: 'ws://localhost/ws',
      token: 'test-token',
      onEvent: (e) => events.push(e),
    });
    await ws.connect();

    const raw = (ws as any).ws as MockWebSocket;
    raw._receive({ type: 'text_delta', delta: 'x' });

    expect(events).toHaveLength(1);
  });

  test('stateChange events fire', async () => {
    const states: string[] = [];
    const ws = new AstraWebSocket({ url: 'ws://localhost/ws', token: 'test-token' });
    ws.on('stateChange', (s) => states.push(s));

    await ws.connect();
    expect(states).toContain('connected');

    ws.close();
    expect(states).toContain('disconnected');
  });
});

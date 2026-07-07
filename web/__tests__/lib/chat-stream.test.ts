import { TextDecoder, TextEncoder } from 'util';
import { streamChatMessage, streamExistingChatRun } from '@/lib/api/chats';
import type { WebApiError } from '@/lib/api/errors';

const defaultPayload = {
  content: 'hello',
  options: {
    webSearch: false,
    thinking: true,
    model: 'sonnet-4.6-adaptive',
    activeSkills: [],
  },
};

function sseBody(frames: string[]) {
  const encoder = new TextEncoder();
  const chunks = frames.map((frame) => encoder.encode(frame));
  let index = 0;
  const cancel = vi.fn();
  const releaseLock = vi.fn();

  return {
    cancel,
    releaseLock,
    getReader() {
      return {
        async read() {
          if (index >= chunks.length) {
            return { value: undefined, done: true };
          }
          const value = chunks[index];
          index += 1;
          return { value, done: false };
        },
        cancel,
        releaseLock,
      };
    },
  };
}

function pendingSseBody() {
  let releasePendingRead: (() => void) | null = null;
  const cancel = vi.fn(() => {
    releasePendingRead?.();
    return Promise.resolve();
  });
  const releaseLock = vi.fn();
  const read = vi.fn(
    () =>
      new Promise<{ value?: Uint8Array; done: boolean }>((resolve) => {
        releasePendingRead = () => resolve({ value: undefined, done: true });
      }),
  );

  return {
    cancel,
    releaseLock,
    getReader() {
      return {
        read,
        cancel,
        releaseLock,
      };
    },
  };
}

describe('streamChatMessage cancellation semantics', () => {
  beforeEach(() => {
    globalThis.fetch = vi.fn();
    globalThis.TextDecoder = TextDecoder as typeof globalThis.TextDecoder;
  });

  it('treats a cancelled run as a clean stop instead of a failed stream', async () => {
    const onCancelled = vi.fn();
    const onDone = vi.fn();
    const onWorkSurfaceEvent = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123"}\n\n',
        'data: {"type":"run_finished","run_id":"run-123","status":"cancelled"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onCancelled,
        onDone,
        onWorkSurfaceEvent,
      }),
    ).resolves.toBe('');

    expect(onCancelled).toHaveBeenCalledWith('');
    expect(onDone).not.toHaveBeenCalled();
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith({
      type: 'run_started',
      run_id: 'run-123',
    });
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith({
      type: 'run_finished',
      run_id: 'run-123',
      status: 'cancelled',
    });
  });

  it('treats a paused run as paused and does not complete the assistant message', async () => {
    const onPaused = vi.fn();
    const onDone = vi.fn();
    const onRunUpdated = vi.fn();
    const onWorkSurfaceEvent = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123"}\n\n',
        'data: {"type":"text_delta","content":"partial"}\n\n',
        'data: {"type":"run_paused","run_id":"run-123"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onPaused,
        onDone,
        onRunUpdated,
        onWorkSurfaceEvent,
      }),
    ).resolves.toBe('partial');

    expect(onRunUpdated).toHaveBeenCalledWith(
      expect.objectContaining({
        runId: 'run-123',
        status: 'paused',
        waitingFor: null,
      }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith({
      type: 'run_paused',
      run_id: 'run-123',
    });
    expect(onPaused).toHaveBeenCalledWith('partial');
    expect(onDone).not.toHaveBeenCalled();
  });

  it('treats an interrupted run as paused and keeps the work surface current', async () => {
    const onPaused = vi.fn();
    const onDone = vi.fn();
    const onRunUpdated = vi.fn();
    const onRunFinished = vi.fn();
    const onWorkSurfaceEvent = vi.fn();
    const onText = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123"}\n\n',
        'data: {"type":"run_interrupted","run_id":"run-123","kind":"budget_exhausted","resumable":true,"message":"Budget exhausted. You can continue."}\n\n',
        'data: {"type":"run_finished","run_id":"run-123","status":"paused","interrupted":true,"resumable":true}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onPaused,
        onDone,
        onRunUpdated,
        onRunFinished,
        onWorkSurfaceEvent,
        onText,
      }),
    ).resolves.toBe('');

    expect(onText).not.toHaveBeenCalled();
    expect(onRunUpdated).toHaveBeenLastCalledWith(
      expect.objectContaining({
        runId: 'run-123',
        status: 'paused',
        waitingFor: 'user_resume',
      }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'run_interrupted', run_id: 'run-123' }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'run_finished',
        run_id: 'run-123',
        status: 'paused',
      }),
    );
    expect(onPaused).toHaveBeenCalledWith('');
    expect(onRunFinished).not.toHaveBeenCalled();
    expect(onDone).not.toHaveBeenCalled();
  });

  it('projects blocked run events into the main active-run state', async () => {
    const onPaused = vi.fn();
    const onDone = vi.fn();
    const onRunUpdated = vi.fn();
    const onWorkSurfaceEvent = vi.fn();
    const onText = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123"}\n\n',
        'data: {"type":"run_blocked","session_id":"session-1","reason":"executor_offline","message":"Edge executor MacBook Pro is offline."}\n\n',
        'data: {"type":"run_waiting","run_id":"run-123","reason":"waiting: executor_offline"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onPaused,
        onDone,
        onRunUpdated,
        onWorkSurfaceEvent,
        onText,
      }),
    ).resolves.toBe('');

    expect(onText).not.toHaveBeenCalled();
    expect(onRunUpdated).toHaveBeenLastCalledWith(
      expect.objectContaining({
        runId: 'run-123',
        status: 'blocked',
        waitingFor: 'executor_offline',
      }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'run_blocked' }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'run_waiting' }),
    );
    expect(onPaused).toHaveBeenCalledWith('');
    expect(onDone).not.toHaveBeenCalled();
  });

  it('projects generic run_waiting events into waiting active-run state', async () => {
    const onPaused = vi.fn();
    const onDone = vi.fn();
    const onRunUpdated = vi.fn();
    const onWorkSurfaceEvent = vi.fn();
    const onText = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123"}\n\n',
        'data: {"type":"run_waiting","run_id":"run-123","reason":"waiting: tool_approval"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onPaused,
        onDone,
        onRunUpdated,
        onWorkSurfaceEvent,
        onText,
      }),
    ).resolves.toBe('');

    expect(onRunUpdated).toHaveBeenLastCalledWith(
      expect.objectContaining({
        runId: 'run-123',
        status: 'waiting',
        waitingFor: 'tool_approval',
      }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'run_waiting' }),
    );
    expect(onText).not.toHaveBeenCalled();
    expect(onPaused).toHaveBeenCalledWith('');
    expect(onDone).not.toHaveBeenCalled();
  });

  it('projects blocked run events into active-run state from reason fields', async () => {
    const onPaused = vi.fn();
    const onDone = vi.fn();
    const onRunUpdated = vi.fn();
    const onWorkSurfaceEvent = vi.fn();
    const onText = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123"}\n\n',
        'data: {"type":"run_blocked","session_id":"session-1","reason":"fallback_disabled","message":"No alternate execution provider is available for this file environment."}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onPaused,
        onDone,
        onRunUpdated,
        onWorkSurfaceEvent,
        onText,
      }),
    ).resolves.toBe('');

    expect(onText).not.toHaveBeenCalled();
    expect(onRunUpdated).toHaveBeenLastCalledWith(
      expect.objectContaining({
        runId: 'run-123',
        status: 'blocked',
        waitingFor: 'fallback_disabled',
      }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'run_blocked' }),
    );
    expect(onPaused).toHaveBeenCalledWith('');
    expect(onDone).not.toHaveBeenCalled();
  });

  it('passes AbortSignal to fetch and releases the reader lock', async () => {
    const body = sseBody([
      'data: {"type":"run_started","run_id":"run-123"}\n\n',
    ]);
    const signal = new AbortController().signal;
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body,
    });

    await streamChatMessage('chat-123', defaultPayload, { signal });

    expect(globalThis.fetch).toHaveBeenCalledWith(
      '/api/chats/chat-123/stream',
      expect.objectContaining({ signal }),
    );
    expect(body.releaseLock).toHaveBeenCalled();
  });

  it('dispatches run, message, reasoning, artifact, and text completion events', async () => {
    const onLocalMessages = vi.fn();
    const onRunStarted = vi.fn();
    const onRunUpdated = vi.fn();
    const onArtifacts = vi.fn();
    const onReasoning = vi.fn();
    const onReasoningDone = vi.fn();
    const onText = vi.fn();
    const onDone = vi.fn();
    const onRunFinished = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"session_info","run_id":"run-session"}\n\n',
        'data: {"type":"local_messages","user_message":{"id":"u1","role":"user","content":"hi","createdAt":"2026-06-07T00:00:00.000Z"},"assistant_message":{"id":"a1","role":"assistant","content":"","createdAt":"2026-06-07T00:00:00.000Z"}}\n\n',
        'data: {"type":"reasoning_delta","content":"think"}\n\n',
        'data: {"type":"thinking_delta","content":"ing"}\n\n',
        'data: {"type":"reasoning_done"}\n\n',
        'data: {"type":"artifacts","artifacts":[{"id":"artifact-1","kind":"text","title":"Notes"}]}\n\n',
        'data: {"type":"text_done","full_text":"<thinking>hidden</thinking>visible"}\n\n',
        'data: {"type":"run_finished","run_id":"run-session","status":"completed"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onLocalMessages,
        onRunStarted,
        onRunUpdated,
        onArtifacts,
        onReasoning,
        onReasoningDone,
        onText,
        onDone,
        onRunFinished,
      }),
    ).resolves.toBe('visible');

    expect(onRunStarted).toHaveBeenCalledWith('run-session');
    expect(onRunUpdated).toHaveBeenCalledWith(
      expect.objectContaining({
        runId: 'run-session',
        status: 'running',
        waitingFor: null,
      }),
    );
    expect(onLocalMessages).toHaveBeenCalledWith({
      userMessage: expect.objectContaining({
        id: 'u1',
        role: 'user',
        content: 'hi',
      }),
      assistantMessage: expect.objectContaining({
        id: 'a1',
        role: 'assistant',
      }),
    });
    expect(onReasoning).toHaveBeenLastCalledWith('thinking');
    expect(onReasoningDone).toHaveBeenCalledWith('thinking');
    expect(onReasoningDone).toHaveBeenLastCalledWith('hidden');
    expect(onArtifacts).toHaveBeenCalledWith([
      expect.objectContaining({ id: 'artifact-1' }),
    ]);
    expect(onText).toHaveBeenLastCalledWith('visible');
    expect(onRunFinished).toHaveBeenCalledWith({
      runId: 'run-session',
      status: 'completed',
      error: null,
    });
    expect(onDone).toHaveBeenCalledWith('visible');
  });

  it('forwards workspace, executor, and transport events to the work surface while streaming', async () => {
    const onWorkSurfaceEvent = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"workspace_bound","workspace":{"kind":"server_sandbox","display_name":"Server sandbox","cwd":"/tmp/astra-workspaces/run-1","authority":"read_write","fallback_policy":"disabled"}}\n\n',
        'data: {"type":"executor_bound","executor":{"kind":"server_local","executor_id":"server-local","display_name":"Server sandbox","transport":"server_local","status":"online"}}\n\n',
        'data: {"type":"tool_routing_decision","call_id":"call-1","tool":"bash","route":"server_local","transport":"server_local"}\n\n',
        'data: {"type":"tool_transport_started","call_id":"call-1","tool":"bash","transport":"server_local"}\n\n',
        'data: {"type":"tool_transport_completed","call_id":"call-1","tool":"bash","transport":"server_local","duration_ms":12}\n\n',
        'data: {"type":"tool_call_end","call_id":"call-1","tool":"bash","success":true,"result":"ok"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onWorkSurfaceEvent,
      }),
    ).resolves.toBe('');

    expect(onWorkSurfaceEvent).toHaveBeenCalledTimes(6);
    expect(onWorkSurfaceEvent).toHaveBeenNthCalledWith(
      1,
      expect.objectContaining({
        type: 'workspace_bound',
        workspace: expect.objectContaining({ kind: 'server_sandbox' }),
      }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenNthCalledWith(
      3,
      expect.objectContaining({
        type: 'tool_routing_decision',
        call_id: 'call-1',
        route: 'server_local',
      }),
    );
  });

  it('forwards run_started bindings to the work surface without dropping run state updates', async () => {
    const onWorkSurfaceEvent = vi.fn();
    const onRunStarted = vi.fn();
    const onRunUpdated = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123","session_id":"session-123","workspace":{"kind":"edge_workspace","display_name":"MacBook Pro","cwd":"/Users/xupeng/github/astra","authority":"read_write","fallback_policy":"disabled"},"executor":{"kind":"edge_agent","executor_id":"edge-macbook-1","display_name":"MacBook Pro","transport":"edge_ws","status":"online"},"transport":"edge_ws","fallback_policy":"disabled"}\n\n',
        'data: {"type":"run_finished","run_id":"run-123","status":"completed"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onWorkSurfaceEvent,
        onRunStarted,
        onRunUpdated,
      }),
    ).resolves.toBe('');

    expect(onWorkSurfaceEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'run_started',
        workspace: expect.objectContaining({ kind: 'edge_workspace' }),
        executor: expect.objectContaining({ kind: 'edge_agent' }),
      }),
    );
    expect(onRunStarted).toHaveBeenCalledWith('run-123');
    expect(onRunUpdated).toHaveBeenCalledWith(
      expect.objectContaining({
        runId: 'run-123',
        status: 'running',
        waitingFor: null,
      }),
    );
  });

  it('uses turn_complete text as the final assistant text', async () => {
    const onText = vi.fn();
    const onDone = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"text_delta","content":"draft"}\n\n',
        'data: {"type":"turn_complete","assistant_text":"final answer"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onText,
        onDone,
      }),
    ).resolves.toBe('final answer');

    expect(onText).toHaveBeenLastCalledWith('final answer');
    expect(onDone).toHaveBeenCalledWith('final answer');
  });

  it('throws stream error events and failed run_finished errors instead of calling done', async () => {
    const onDone = vi.fn();
    const onRunFinished = vi.fn();
    const onRunUpdated = vi.fn();
    const onWorkSurfaceEvent = vi.fn();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      ok: true,
      body: sseBody([
        'data: {"type":"error","message":"runtime disconnected"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onDone,
      }),
    ).rejects.toThrow('runtime disconnected');
    expect(onDone).not.toHaveBeenCalled();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      ok: true,
      body: sseBody([
        'data: {"type":"run_finished","run_id":"run-123","status":"failed","error":"tool crashed"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onDone,
        onRunFinished,
      }),
    ).rejects.toThrow('tool crashed');
    expect(onRunFinished).toHaveBeenCalledWith({
      runId: 'run-123',
      status: 'failed',
      error: 'tool crashed',
    });
    expect(onDone).not.toHaveBeenCalled();

    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      ok: true,
      body: sseBody([
        'data: {"type":"run_error","run_id":"run-456","message":"loop crashed","error_kind":"runtime"}\n\n',
        'data: {"type":"run_finished","run_id":"run-456","status":"failed","error":"loop crashed"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        onDone,
        onRunUpdated,
        onWorkSurfaceEvent,
      }),
    ).rejects.toThrow('loop crashed');
    expect(onRunUpdated).toHaveBeenCalledWith(
      expect.objectContaining({
        runId: 'run-456',
        status: 'failed',
        waitingFor: null,
      }),
    );
    expect(onWorkSurfaceEvent).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'run_error', run_id: 'run-456' }),
    );
    expect(onDone).not.toHaveBeenCalled();
  });

  it('cancels the reader when the signal is already aborted', async () => {
    const body = sseBody([
      'data: {"type":"run_started","run_id":"run-123"}\n\n',
    ]);
    const controller = new AbortController();
    controller.abort();
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body,
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {
        signal: controller.signal,
      }),
    ).rejects.toMatchObject({ name: 'AbortError' });

    expect(body.cancel).toHaveBeenCalled();
    expect(body.releaseLock).toHaveBeenCalled();
  });

  it('cancels the reader when the signal aborts during a pending read', async () => {
    const body = pendingSseBody();
    const controller = new AbortController();
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body,
    });

    const stream = streamChatMessage('chat-123', defaultPayload, {
      signal: controller.signal,
    });
    await Promise.resolve();

    controller.abort();

    await waitUntil(() => {
      expect(body.cancel).toHaveBeenCalled();
    });
    await expect(stream).rejects.toMatchObject({ name: 'AbortError' });
    expect(body.releaseLock).toHaveBeenCalled();
  });

  it('streams an existing run with an encoded GET URL', async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody(['data: {"type":"text_done","full_text":"resumed"}\n\n']),
    });

    await expect(
      streamExistingChatRun('chat 123', 'run/123', {}),
    ).resolves.toBe('resumed');

    expect(globalThis.fetch).toHaveBeenCalledWith(
      '/api/chats/chat%20123/stream?runId=run%2F123',
      { method: 'GET', signal: undefined },
    );
  });

  it('streams an existing run from the stored cursor into the bound assistant message', async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody(['data: {"type":"text_done","full_text":"resumed"}\n\n']),
    });

    await expect(
      streamExistingChatRun(
        'chat 123',
        'run/123',
        {},
        {
          assistantMessageId: 'assistant-queued',
          nextEventIndex: 9,
        },
      ),
    ).resolves.toBe('resumed');

    expect(globalThis.fetch).toHaveBeenCalledWith(
      '/api/chats/chat%20123/stream?runId=run%2F123&last_index=9&assistantMessageId=assistant-queued',
      { method: 'GET', signal: undefined },
    );
  });

  it('ignores malformed event indexes without aborting stream consumption', async () => {
    const onDone = vi.fn();
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_input_queued","run_id":"run-123","index":"4"}\n\n',
        'data: {"type":"text_done","full_text":"continued"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, { onDone }),
    ).resolves.toBe('continued');
    expect(onDone).toHaveBeenCalledWith('continued');
  });

  it('preserves structured stream error status and code', async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"error","message":"Edge selection is stale.","status":409,"code":"workspace_edge_stale_selection"}\n\n',
      ]),
    });

    await expect(
      streamChatMessage('chat-123', defaultPayload, {}),
    ).rejects.toMatchObject({
      status: 409,
      detail: 'Edge selection is stale.',
      code: 'workspace_edge_stale_selection',
    } satisfies Partial<WebApiError>);
  });
});

async function waitUntil(assertion: () => void) {
  for (let attempt = 0; attempt < 20; attempt += 1) {
    try {
      assertion();
      return;
    } catch (error) {
      if (attempt === 19) {
        throw error;
      }
      await new Promise((resolve) => {
        setTimeout(resolve, 0);
      });
    }
  }
}

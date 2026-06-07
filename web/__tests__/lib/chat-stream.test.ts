import { TextDecoder, TextEncoder } from 'util';
import { streamChatMessage, streamExistingChatRun } from '@/lib/api/chats';

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
  const cancel = jest.fn();
  const releaseLock = jest.fn();

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

describe('streamChatMessage cancellation semantics', () => {
  beforeEach(() => {
    globalThis.fetch = jest.fn();
    globalThis.TextDecoder = TextDecoder as typeof globalThis.TextDecoder;
  });

  it('treats a cancelled run as a clean stop instead of a failed stream', async () => {
    const onCancelled = jest.fn();
    const onDone = jest.fn();

    (globalThis.fetch as jest.Mock).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123"}\n\n',
        'data: {"type":"run_finished","run_id":"run-123","status":"cancelled"}\n\n',
      ]),
    });

    await expect(streamChatMessage('chat-123', defaultPayload, {
      onCancelled,
      onDone,
    })).resolves.toBe('');

    expect(onCancelled).toHaveBeenCalledWith('');
    expect(onDone).not.toHaveBeenCalled();
  });

  it('treats a paused run as paused and does not complete the assistant message', async () => {
    const onPaused = jest.fn();
    const onDone = jest.fn();
    const onRunUpdated = jest.fn();

    (globalThis.fetch as jest.Mock).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"run_started","run_id":"run-123"}\n\n',
        'data: {"type":"text_delta","content":"partial"}\n\n',
        'data: {"type":"run_paused","run_id":"run-123"}\n\n',
      ]),
    });

    await expect(streamChatMessage('chat-123', defaultPayload, {
      onPaused,
      onDone,
      onRunUpdated,
    })).resolves.toBe('partial');

    expect(onRunUpdated).toHaveBeenCalledWith({ runId: 'run-123', status: 'paused', waitingFor: null });
    expect(onPaused).toHaveBeenCalledWith('partial');
    expect(onDone).not.toHaveBeenCalled();
  });

  it('passes AbortSignal to fetch and releases the reader lock', async () => {
    const body = sseBody([
      'data: {"type":"run_started","run_id":"run-123"}\n\n',
    ]);
    const signal = new AbortController().signal;
    (globalThis.fetch as jest.Mock).mockResolvedValue({
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
    const onLocalMessages = jest.fn();
    const onRunStarted = jest.fn();
    const onRunUpdated = jest.fn();
    const onArtifacts = jest.fn();
    const onReasoning = jest.fn();
    const onReasoningDone = jest.fn();
    const onText = jest.fn();
    const onDone = jest.fn();
    const onRunFinished = jest.fn();

    (globalThis.fetch as jest.Mock).mockResolvedValue({
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

    await expect(streamChatMessage('chat-123', defaultPayload, {
      onLocalMessages,
      onRunStarted,
      onRunUpdated,
      onArtifacts,
      onReasoning,
      onReasoningDone,
      onText,
      onDone,
      onRunFinished,
    })).resolves.toBe('visible');

    expect(onRunStarted).toHaveBeenCalledWith('run-session');
    expect(onRunUpdated).toHaveBeenCalledWith({ runId: 'run-session', status: 'running', waitingFor: null });
    expect(onLocalMessages).toHaveBeenCalledWith({
      userMessage: expect.objectContaining({ id: 'u1', role: 'user', content: 'hi' }),
      assistantMessage: expect.objectContaining({ id: 'a1', role: 'assistant' }),
    });
    expect(onReasoning).toHaveBeenLastCalledWith('thinking');
    expect(onReasoningDone).toHaveBeenCalledWith('thinking');
    expect(onReasoningDone).toHaveBeenLastCalledWith('hidden');
    expect(onArtifacts).toHaveBeenCalledWith([expect.objectContaining({ id: 'artifact-1' })]);
    expect(onText).toHaveBeenLastCalledWith('visible');
    expect(onRunFinished).toHaveBeenCalledWith({ runId: 'run-session', status: 'completed', error: null });
    expect(onDone).toHaveBeenCalledWith('visible');
  });

  it('uses turn_complete text as the final assistant text', async () => {
    const onText = jest.fn();
    const onDone = jest.fn();

    (globalThis.fetch as jest.Mock).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"text_delta","content":"draft"}\n\n',
        'data: {"type":"turn_complete","assistant_text":"final answer"}\n\n',
      ]),
    });

    await expect(streamChatMessage('chat-123', defaultPayload, {
      onText,
      onDone,
    })).resolves.toBe('final answer');

    expect(onText).toHaveBeenLastCalledWith('final answer');
    expect(onDone).toHaveBeenCalledWith('final answer');
  });

  it('throws stream error events and failed run_finished errors instead of calling done', async () => {
    const onDone = jest.fn();
    const onRunFinished = jest.fn();

    (globalThis.fetch as jest.Mock).mockResolvedValueOnce({
      ok: true,
      body: sseBody([
        'data: {"type":"error","message":"runtime disconnected"}\n\n',
      ]),
    });

    await expect(streamChatMessage('chat-123', defaultPayload, {
      onDone,
    })).rejects.toThrow('runtime disconnected');
    expect(onDone).not.toHaveBeenCalled();

    (globalThis.fetch as jest.Mock).mockResolvedValueOnce({
      ok: true,
      body: sseBody([
        'data: {"type":"run_finished","run_id":"run-123","status":"failed","error":"tool crashed"}\n\n',
      ]),
    });

    await expect(streamChatMessage('chat-123', defaultPayload, {
      onDone,
      onRunFinished,
    })).rejects.toThrow('tool crashed');
    expect(onRunFinished).toHaveBeenCalledWith({ runId: 'run-123', status: 'failed', error: 'tool crashed' });
    expect(onDone).not.toHaveBeenCalled();
  });

  it('cancels the reader when the signal is already aborted', async () => {
    const body = sseBody([
      'data: {"type":"run_started","run_id":"run-123"}\n\n',
    ]);
    const controller = new AbortController();
    controller.abort();
    (globalThis.fetch as jest.Mock).mockResolvedValue({
      ok: true,
      body,
    });

    await expect(streamChatMessage('chat-123', defaultPayload, {
      signal: controller.signal,
    })).rejects.toMatchObject({ name: 'AbortError' });

    expect(body.cancel).toHaveBeenCalled();
    expect(body.releaseLock).toHaveBeenCalled();
  });

  it('streams an existing run with an encoded GET URL', async () => {
    (globalThis.fetch as jest.Mock).mockResolvedValue({
      ok: true,
      body: sseBody([
        'data: {"type":"text_done","full_text":"resumed"}\n\n',
      ]),
    });

    await expect(streamExistingChatRun('chat 123', 'run/123', {})).resolves.toBe('resumed');

    expect(globalThis.fetch).toHaveBeenCalledWith(
      '/api/chats/chat%20123/stream?runId=run%2F123',
      { method: 'GET', signal: undefined },
    );
  });
});

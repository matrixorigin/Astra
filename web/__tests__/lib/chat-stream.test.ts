import { TextDecoder, TextEncoder } from 'util';
import { streamChatMessage } from '@/lib/api/chats';

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

    await expect(streamChatMessage('chat-123', {
      content: 'stop this',
      options: {
        webSearch: false,
        thinking: true,
        model: 'sonnet-4.6-adaptive',
        activeSkills: [],
      },
    }, {
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

    await expect(streamChatMessage('chat-123', {
      content: 'pause this',
      options: {
        webSearch: false,
        thinking: true,
        model: 'sonnet-4.6-adaptive',
        activeSkills: [],
      },
    }, {
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

    await streamChatMessage('chat-123', {
      content: 'hello',
      options: {
        webSearch: false,
        thinking: true,
        model: 'sonnet-4.6-adaptive',
        activeSkills: [],
      },
    }, { signal });

    expect(globalThis.fetch).toHaveBeenCalledWith(
      '/api/chats/chat-123/stream',
      expect.objectContaining({ signal }),
    );
    expect(body.releaseLock).toHaveBeenCalled();
  });
});

import { TextDecoder, TextEncoder } from 'util';
import { streamChatMessage } from '@/lib/api/chats';

function sseBody(frames: string[]) {
  const encoder = new TextEncoder();
  const chunks = frames.map((frame) => encoder.encode(frame));
  let index = 0;

  return {
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
        releaseLock() {},
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
});

import { isChatScrolledToBottom, shouldAutoScrollChat } from '@/lib/chat-scroll-state';

describe('chat scroll state', () => {
  it('treats the viewport as pinned when it is near the bottom', () => {
    expect(isChatScrolledToBottom({
      scrollHeight: 1000,
      scrollTop: 430,
      clientHeight: 500,
    })).toBe(true);
  });

  it('treats manual scrollback as unpinned before deferred messages arrive', () => {
    expect(isChatScrolledToBottom({
      scrollHeight: 1000,
      scrollTop: 200,
      clientHeight: 500,
    })).toBe(false);
  });

  it('only auto-scrolls when the user is pinned to the bottom', () => {
    expect(shouldAutoScrollChat({ pinnedToBottom: true })).toBe(true);
    expect(shouldAutoScrollChat({ pinnedToBottom: false })).toBe(false);
  });
});

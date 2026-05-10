import type { ChatDetail } from '@/lib/api/types';

export type ChatLifecycleAction = 'archive' | 'unarchive' | 'delete' | 'clearArchived';

export type ChatLifecycleEventDetail = {
  action: ChatLifecycleAction;
  chatId?: string;
  archived?: boolean;
  chat?: ChatDetail;
};

const CHAT_LIFECYCLE_EVENT = 'astra:chat-lifecycle';

export function emitChatLifecycleChange(detail: ChatLifecycleEventDetail) {
  if (typeof window === 'undefined') {
    return;
  }
  window.dispatchEvent(new CustomEvent<ChatLifecycleEventDetail>(CHAT_LIFECYCLE_EVENT, { detail }));
}

export function subscribeChatLifecycleChange(
  listener: (detail: ChatLifecycleEventDetail) => void,
) {
  if (typeof window === 'undefined') {
    return () => {};
  }

  const handler = (event: Event) => {
    listener((event as CustomEvent<ChatLifecycleEventDetail>).detail);
  };
  window.addEventListener(CHAT_LIFECYCLE_EVENT, handler);
  return () => window.removeEventListener(CHAT_LIFECYCLE_EVENT, handler);
}
